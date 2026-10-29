//! Backward-compatible session listing with an opt-in creation-order cursor.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    created_at: String,
    id: String,
}

pub(super) fn list(store: &Store, usage: &UsageSettings, params: &Value) -> Result<Value> {
    let mut sessions = store.list_visible_sessions()?;
    if params.get("limit").is_none() && params.get("cursor").is_none() {
        return serde_json::to_value(
            sessions
                .iter()
                .map(|session| session_with_usage(store, session, usage))
                .collect::<Result<Vec<_>>>()?,
        )
        .map_err(Into::into);
    }
    let limit = match params.get("limit") {
        None => 100,
        Some(value) => {
            let limit = value.as_u64().context("limit must be a positive integer")?;
            ensure!(
                (1..=1000).contains(&limit),
                "limit must be between 1 and 1000"
            );
            limit as usize
        }
    };
    let cursor = match params.get("cursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => {
            ensure!(value.len() <= 2048, "invalid session cursor");
            let bytes = URL_SAFE_NO_PAD
                .decode(value)
                .context("invalid session cursor")?;
            let cursor: Cursor =
                serde_json::from_slice(&bytes).context("invalid session cursor")?;
            ensure!(
                cursor.version == 1
                    && !cursor.id.is_empty()
                    && cursor.id.len() <= 256
                    && !cursor.id.chars().any(char::is_control)
                    && chrono::DateTime::parse_from_rfc3339(&cursor.created_at).is_ok(),
                "invalid session cursor"
            );
            Some(cursor)
        }
        _ => bail!("cursor must be a string or null"),
    };
    // Creation metadata is immutable: later activity and renames cannot move a
    // session between pages. ID breaks timestamp ties deterministically.
    sessions.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    let mut remaining = sessions.iter().filter(|session| {
        cursor.as_ref().is_none_or(|cursor| {
            (&session.created_at, &session.id) < (&cursor.created_at, &cursor.id)
        })
    });
    let page = remaining.by_ref().take(limit).collect::<Vec<_>>();
    let next_cursor = if remaining.next().is_some() {
        let last = page
            .last()
            .expect("a nonempty limit precedes remaining rows");
        Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor {
            version: 1,
            created_at: last.created_at.clone(),
            id: last.id.clone(),
        })?))
    } else {
        None
    };
    let sessions = page
        .into_iter()
        .map(|session| session_with_usage(store, session, usage))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"sessions":sessions,"next_cursor":next_cursor}))
}
