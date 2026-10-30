use super::*;
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    created_at: String,
    id: String,
}
#[derive(Deserialize, Serialize)]
pub struct ListParams {
    pub limit: Option<u64>,
    pub cursor: Option<String>,
}
#[derive(Deserialize, Serialize)]
pub struct HistoryParams {
    pub session_id: String,
    pub before: Option<String>,
    pub limit: Option<usize>,
}
#[derive(Serialize, Deserialize)]
pub struct MessagesPage {
    pub messages: Vec<bubblepaw_core::Message>,
    pub has_more: bool,
}
#[derive(Deserialize, Serialize)]
pub struct SearchParams {
    pub query: String,
    #[serde(default, alias = "chat")]
    pub session_id: Option<String>,
    pub limit: Option<usize>,
}
#[derive(Deserialize, Serialize)]
pub struct ImportParams {
    pub path: PathBuf,
    pub format: bubblepaw_core::ImportFormat,
    pub title: Option<String>,
    #[serde(default)]
    pub children: Vec<PathBuf>,
}
#[derive(Deserialize, Serialize)]
pub struct SessionHistoryParams {
    pub session_id: String,
    #[serde(default)]
    pub after_seq: i64,
    pub limit: Option<usize>,
}
#[derive(Serialize, Deserialize)]
pub struct HistoryPage {
    pub messages: Vec<bubblepaw_core::Message>,
    pub next_seq: Option<i64>,
}
#[derive(Serialize, Deserialize)]
pub struct SessionWithUsage {
    #[serde(flatten)]
    pub session: bubblepaw_core::Session,
    pub usage: Value,
}
#[derive(Serialize, Deserialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionWithUsage>,
    pub next_cursor: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum ListResult {
    All(Vec<SessionWithUsage>),
    Page(SessionPage),
}
#[derive(Serialize, Deserialize)]
pub struct ArchiveResult {
    pub chat: String,
    pub changed: bool,
    pub worktrees: Vec<crate::worktrees::CleanupReport>,
}
#[derive(Serialize, Deserialize)]
pub struct UnarchiveResult {
    pub chat: String,
    pub storage_state: String,
}
pub type OpenResult = bubblepaw_core::Session;
pub type SearchResult = Vec<bubblepaw_core::SearchHit>;
pub type ImportResult = bubblepaw_core::ImportResult;
pub type TodoResult = Vec<crate::todo::TodoItem>;
/// Protocol 0.1's snapshot contains independently evolving host projections.
pub type StateResult = Value;

fn with_usage(
    ctx: &HostCtx,
    session: &bubblepaw_core::Session,
) -> anyhow::Result<SessionWithUsage> {
    let usage = if ctx.store.active_session_metadata(&session.id)?.is_some() {
        serde_json::to_value(crate::usage::session_usage(
            &ctx.store,
            &session.id,
            &ctx.usage_settings(),
        )?)?
    } else {
        Value::Null
    };
    Ok(SessionWithUsage {
        session: session.clone(),
        usage,
    })
}

fn list_wire(ctx: &HostCtx, params: Value) -> Result<Value, HostError> {
    let params = if params.is_object() {
        params
    } else {
        json!({})
    };
    if params.get("limit").is_some_and(Value::is_null) {
        return Err(HostError::invalid("limit must be a positive integer"));
    }
    let paged = params.get("limit").is_some() || params.get("cursor").is_some();
    let input: ListParams = decode(params)?;
    encode(list_inner(ctx, input, paged)?)
}
/// An omitted limit and cursor request the legacy array; either option enables
/// the creation-order page result.
pub fn list(ctx: &HostCtx, input: ListParams) -> Result<ListResult, HostError> {
    let paged = input.limit.is_some() || input.cursor.is_some();
    list_inner(ctx, input, paged)
}
fn list_inner(ctx: &HostCtx, input: ListParams, paged: bool) -> Result<ListResult, HostError> {
    let mut sessions = Vec::new();
    for session in ctx.store.list_visible_sessions()? {
        if ctx.read_scope.allows(&ctx.store, &session.id)? {
            sessions.push(session);
        }
    }
    if !paged {
        return Ok(ListResult::All(
            sessions
                .iter()
                .map(|s| with_usage(ctx, s))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ));
    }
    let limit = input.limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err(HostError::invalid("limit must be between 1 and 1000"));
    }
    let cursor = input
        .cursor
        .map(|value| -> anyhow::Result<Cursor> {
            ensure!(value.len() <= 2048, "invalid session cursor");
            let cursor: Cursor = serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(value)
                    .context("invalid session cursor")?,
            )
            .context("invalid session cursor")?;
            ensure!(
                cursor.version == 1
                    && !cursor.id.is_empty()
                    && cursor.id.len() <= 256
                    && !cursor.id.chars().any(char::is_control)
                    && chrono::DateTime::parse_from_rfc3339(&cursor.created_at).is_ok(),
                "invalid session cursor"
            );
            Ok(cursor)
        })
        .transpose()
        .map_err(|e| HostError::invalid(e.to_string()))?;
    sessions.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    let mut remaining = sessions.iter().filter(|s| {
        cursor
            .as_ref()
            .is_none_or(|c| (&s.created_at, &s.id) < (&c.created_at, &c.id))
    });
    let page = remaining.by_ref().take(limit as usize).collect::<Vec<_>>();
    let next_cursor = if remaining.next().is_some() {
        let last = page.last().expect("nonempty limit before remaining rows");
        Some(
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&Cursor {
                    version: 1,
                    created_at: last.created_at.clone(),
                    id: last.id.clone(),
                })
                .map_err(anyhow::Error::from)?,
            ),
        )
    } else {
        None
    };
    let sessions = page
        .into_iter()
        .map(|s| with_usage(ctx, s))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(ListResult::Page(SessionPage {
        sessions,
        next_cursor,
    }))
}

pub use super::session_ops::{archive, history, import, messages, open, search, state, unarchive};
pub(super) fn command(ctx: &HostCtx, method: MethodId, params: Value) -> Result<Value, HostError> {
    match method {
        MethodId::SessionList => list_wire(ctx, params),
        MethodId::SessionImport => encode(import(ctx, decode(params)?)?),
        MethodId::SessionOpen => {
            // Legacy aliases prefer a valid primary even when both are supplied.
            let mut normalized = if params.is_object() {
                params.clone()
            } else {
                json!({})
            };
            if let Some(object) = normalized.as_object_mut() {
                let title = params["title"].as_str().or_else(|| params["name"].as_str());
                let parent = params["parent_session_id"]
                    .as_str()
                    .or_else(|| params["parent_id"].as_str());
                object.remove("name");
                object.remove("parent_id");
                object.insert("title".into(), json!(title));
                object.insert("parent_session_id".into(), json!(parent));
                object.insert("session_id".into(), json!(params["session_id"].as_str()));
            }
            encode(open(ctx, decode(normalized)?)?)
        }
        MethodId::SessionSearch => encode(search(ctx, decode(params)?)?),
        MethodId::SessionState => state(ctx, decode(params)?),
        MethodId::SessionHistory => encode(history(ctx, decode(params)?)?),
        MethodId::HistoryMessages => encode(messages(ctx, decode(params)?)?),
        MethodId::SessionArchive => encode(archive(ctx, decode(params)?)?),
        MethodId::SessionUnarchive => encode(unarchive(ctx, decode(params)?)?),
        MethodId::TodoList => {
            let input: SessionParams = decode(params)?;
            encode(todo(ctx, &input.session_id)?)
        }
        MethodId::QuestionPendingChild => {
            let input: SessionParams = decode(params)?;
            encode(super::questions::pending_child(ctx, &input.session_id)?)
        }
        MethodId::SessionFork => encode(super::fork::fork_session(ctx, decode(params)?)?),
        MethodId::ContextView
        | MethodId::ContextStructure
        | MethodId::ContextSummary
        | MethodId::ContextEvents
        | MethodId::HistoryRange => super::context::read(ctx, method, params),
        _ => unreachable!("session method table"),
    }
}
pub fn todo(ctx: &HostCtx, session_id: &str) -> Result<TodoResult, HostError> {
    ctx.ensure_read_session(session_id)?;
    crate::todo::list(&ctx.store, session_id).map_err(Into::into)
}
