use super::views::{SessionSummary, SessionView};
use super::*;
use rusqlite::{Connection, OptionalExtension, params};

const VIEW_LIMIT: usize = 4;
const VIEW_BYTES: usize = 16 * 1024 * 1024;

/// Disposable serving projection; the canonical store remains authoritative.
pub struct ServingView {
    pub(crate) db: Connection,
    pub(crate) list_revision: Option<u64>,
    list_scope: Option<ReadScope>,
    provider_generation: Option<u64>,
}
impl ServingView {
    pub fn new() -> anyhow::Result<Self> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE sidebar(id TEXT PRIMARY KEY, data TEXT NOT NULL); CREATE TABLE views(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, data TEXT NOT NULL);")?;
        Ok(Self {
            db,
            list_revision: None,
            list_scope: None,
            provider_generation: None,
        })
    }
}

pub(super) fn sidebar(
    ctx: &HostCtx,
    serving: &mut ServingView,
) -> Result<(Vec<SessionSummary>, u64), HostError> {
    if serving.list_revision != Some(ctx.store.revision())
        || serving.list_scope.as_ref() != Some(&ctx.read_scope)
    {
        let (summaries, revision) = super::views::persisted_sidebar(ctx)?;
        let tx = serving.db.transaction().map_err(anyhow::Error::from)?;
        tx.execute("DELETE FROM sidebar", [])
            .map_err(anyhow::Error::from)?;
        for item in summaries {
            tx.execute(
                "INSERT INTO sidebar VALUES(?1,?2)",
                params![
                    item.id,
                    serde_json::to_string(&item).map_err(anyhow::Error::from)?
                ],
            )
            .map_err(anyhow::Error::from)?;
        }
        tx.commit().map_err(anyhow::Error::from)?;
        serving.list_revision = Some(revision);
        serving.list_scope = Some(ctx.read_scope.clone());
    }
    let mut statement = serving
        .db
        .prepare("SELECT data FROM sidebar ORDER BY json_extract(data,'$.updated_at') DESC")
        .map_err(anyhow::Error::from)?;
    let data = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(anyhow::Error::from)?;
    let summaries = data
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((summaries, serving.list_revision.unwrap_or_default()))
}

pub(super) fn session(
    ctx: &HostCtx,
    serving: &mut ServingView,
    id: &str,
) -> Result<(SessionView, u64), HostError> {
    let generation = ctx.active_providers().map(|active| active.snapshot().id());
    if serving.provider_generation != generation {
        serving
            .db
            .execute("DELETE FROM views", [])
            .map_err(anyhow::Error::from)?;
        serving.provider_generation = generation;
    }
    let revision = ctx.store.revision();
    let cached: Option<(u64, String)> = serving
        .db
        .query_row("SELECT revision,data FROM views WHERE id=?1", [id], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get(1)?))
        })
        .optional()
        .map_err(anyhow::Error::from)?;
    if let Some((version, data)) = cached.filter(|(version, _)| *version == revision) {
        return Ok((
            serde_json::from_str(&data).map_err(anyhow::Error::from)?,
            version,
        ));
    }
    let (view, revision) = super::views::persisted_session(ctx, id)?;
    let data = serde_json::to_string(&view).map_err(anyhow::Error::from)?;
    serving
        .db
        .execute("DELETE FROM views WHERE id=?1", [id])
        .map_err(anyhow::Error::from)?;
    if data.len() <= VIEW_BYTES {
        serving
            .db
            .execute(
                "INSERT INTO views VALUES(?1,?2,?3)",
                params![id, revision as i64, data],
            )
            .map_err(anyhow::Error::from)?;
        serving.db.execute("DELETE FROM views WHERE rowid NOT IN (SELECT rowid FROM views ORDER BY rowid DESC LIMIT ?1)",[VIEW_LIMIT as i64]).map_err(anyhow::Error::from)?;
        while serving
            .db
            .query_row(
                "SELECT COALESCE(SUM(length(CAST(data AS BLOB))),0) FROM views",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(anyhow::Error::from)?
            > VIEW_BYTES as i64
        {
            serving
                .db
                .execute(
                    "DELETE FROM views WHERE rowid=(SELECT MIN(rowid) FROM views)",
                    [],
                )
                .map_err(anyhow::Error::from)?;
        }
    }
    Ok((view, revision))
}

pub(super) fn refresh(ctx: &HostCtx, serving: &mut ServingView) -> Result<(), HostError> {
    sidebar(ctx, serving)?;
    let ids: Vec<String> = {
        let mut statement = serving
            .db
            .prepare("SELECT id FROM views")
            .map_err(anyhow::Error::from)?;
        statement
            .query_map([], |row| row.get(0))
            .map_err(anyhow::Error::from)?
            .collect::<rusqlite::Result<_>>()
            .map_err(anyhow::Error::from)?
    };
    for id in ids {
        if ctx.read_scope.allows(&ctx.store, &id)? {
            session(ctx, serving, &id)?;
        }
    }
    Ok(())
}
