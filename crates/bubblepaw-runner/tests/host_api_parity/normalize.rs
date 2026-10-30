//! Normalization applied to one transport's reply before comparison. Only values
//! the store or the clock generate are masked: IDs, timestamps, sequence and
//! revision counters, and measured elapsed time. Configured values, the
//! JSON-RPC envelope and the transport's own labels are compared exactly. The
//! role tag in fixture titles is the one label unified, because each transport
//! writes its own titles.
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap()
});
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})").unwrap()
});

/// Keys whose string value is a store-generated identifier.
fn is_id_key(key: &str) -> bool {
    matches!(key, "id" | "sessionId" | "chat" | "deleted" | "handle") || key.ends_with("_id")
}

/// Keys whose number is a store-generated counter.
fn is_sequence_key(key: &str) -> bool {
    matches!(
        key,
        "seq"
            | "sequence"
            | "last_seq"
            | "first_seq"
            | "last_sequence"
            | "next_seq"
            | "event_sequence"
            | "revision"
    )
}

/// Keys whose number is a generated timestamp, or a measured duration.
fn is_time_key(key: &str) -> bool {
    key.ends_with("_at") || matches!(key, "elapsed" | "elapsed_ms" | "duration" | "duration_ms")
}

/// A whole JSON-RPC reply. The envelope (`jsonrpc`, `id`) is exact: a reply
/// must answer its request, so its id is compared, not masked.
pub fn response(reply: &Value, role: &str) -> Value {
    let mut shown = reply.clone();
    for key in ["result", "error"] {
        if let Some(body) = reply.get(key) {
            shown[key] = walk(body, None, role);
        }
    }
    shown
}

fn walk(value: &Value, key: Option<&str>, role: &str) -> Value {
    if let Some(key) = key {
        if key == "next_cursor" && value.is_string() {
            return json!("<cursor>");
        }
        if is_id_key(key) && value.is_string() {
            return json!("<id>");
        }
        if is_sequence_key(key) && value.is_number() {
            return json!("<seq>");
        }
        if is_time_key(key) && value.is_number() {
            return json!("<time>");
        }
    }
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(name, child)| (name.clone(), walk(child, Some(name), role)))
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| walk(item, key, role)).collect())
        }
        Value::String(text) => Value::String(text_of(text, role)),
        other => other.clone(),
    }
}

fn text_of(text: &str, role: &str) -> String {
    let text = UUID.replace_all(text, "<uuid>").into_owned();
    let text = TIMESTAMP.replace_all(&text, "<time>").into_owned();
    text.replace(&format!("parity-{role}"), "parity-<role>")
}

/// Keep only this role's session rows, so a global listing compares like with like.
pub fn role_sessions(result: &Value, role: &str) -> Value {
    let marker = format!("parity-{role}");
    let keep = |rows: &Value| {
        Value::Array(
            rows.as_array()
                .map(|rows| {
                    rows.iter()
                        .filter(|row| {
                            row["title"]
                                .as_str()
                                .is_some_and(|title| title.contains(&marker))
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
        )
    };
    match result {
        Value::Array(_) => keep(result),
        Value::Object(map) => {
            let mut shown = map.clone();
            if let Some(sessions) = map.get("sessions") {
                shown.insert("sessions".into(), keep(sessions));
            }
            Value::Object(shown)
        }
        other => other.clone(),
    }
}

/// The structure of a reply with its values replaced by their types. Used only
/// for store-wide counters that differ between the two phases; the same-store
/// test compares those values exactly.
pub fn shape(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(name, child)| (name.clone(), shape(child)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.first().map(shape).into_iter().collect()),
        Value::Null => Value::Null,
        Value::Bool(_) => json!("<bool>"),
        Value::Number(_) => json!("<number>"),
        Value::String(_) => json!("<string>"),
    }
}
