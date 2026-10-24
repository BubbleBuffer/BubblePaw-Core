//! Lossless provider replay metadata. Message text and calls remain the sole content copy.
use anyhow::{Result, anyhow, ensure};
use bubblepaw_core::ToolCall;
use serde_json::{Value, json};

const FORMAT: &str = "message-references-v1";

pub(crate) fn slim(mut state: Value, text: &str, calls: &[ToolCall]) -> Result<Value> {
    let responses = state["adapter"] == "responses";
    let field = if responses { "items" } else { "content" };
    let mut offset = 0;
    let mut refs = Vec::new();
    for block in state[field]
        .as_array_mut()
        .ok_or_else(|| anyhow!("missing replay blocks"))?
    {
        let mut reference = json!({});
        if responses && block["type"] == "message" {
            let mut ranges = Vec::new();
            if let Some(parts) = block["content"].as_array_mut() {
                for part in parts {
                    if part["type"] == "output_text" && part["text"].is_string() {
                        ranges.push(take_text(part, text, &mut offset)?);
                    } else {
                        ranges.push(Value::Null);
                    }
                }
            }
            reference["text"] = json!(ranges);
        } else if !responses && block["type"] == "text" {
            reference["text"] = take_text(block, text, &mut offset)?;
        }
        let is_call = block["type"]
            == if responses {
                "function_call"
            } else {
                "tool_use"
            };
        if is_call {
            let id_field = if responses { "call_id" } else { "id" };
            if let Some(index) = calls.iter().position(|call| block[id_field] == call.id) {
                reference["call"] = json!(index);
                if responses {
                    let canonical = calls[index].arguments.to_string();
                    if let Some(raw) = block["arguments"].as_str() {
                        if raw != canonical {
                            reference["arguments"] = json_layout(raw, &canonical);
                        }
                    } else if let Some(value) = block.get("arguments") {
                        reference["arguments_verbatim"] = value.clone();
                    } else {
                        // Absent arguments are distinct from an explicit serialized object.
                        reference["arguments_absent"] = json!(true);
                    }
                }
                let object = block
                    .as_object_mut()
                    .ok_or_else(|| anyhow!("invalid replay block"))?;
                for key in [
                    id_field,
                    "name",
                    if responses { "arguments" } else { "input" },
                ] {
                    object.remove(key);
                }
            }
        }
        refs.push(reference);
    }
    ensure!(
        offset == text.len(),
        "replay text differs from message text"
    );
    state["format"] = json!(FORMAT);
    state["refs"] = json!(refs);
    Ok(state)
}

fn take_text(block: &mut Value, text: &str, offset: &mut usize) -> Result<Value> {
    let value = block["text"]
        .as_str()
        .ok_or_else(|| anyhow!("missing replay text"))?;
    let end = *offset + value.len();
    ensure!(
        text.get(*offset..end) == Some(value),
        "replay text differs from message text"
    );
    let range = json!([*offset, end]);
    *offset = end;
    block.as_object_mut().unwrap().remove("text");
    Ok(range)
}

pub(crate) fn hydrate(state: &Value, text: &str, calls: &[ToolCall]) -> Result<Vec<Value>> {
    let responses = state["adapter"] == "responses";
    let field = if responses { "items" } else { "content" };
    let mut blocks = state[field]
        .as_array()
        .ok_or_else(|| anyhow!("missing replay blocks"))?
        .clone();
    // Legacy archives retain their original provider wire representation unchanged.
    if state["format"] != FORMAT {
        return Ok(blocks);
    }
    let refs = state["refs"]
        .as_array()
        .ok_or_else(|| anyhow!("missing replay references"))?;
    ensure!(refs.len() == blocks.len(), "invalid replay reference count");
    for (block, reference) in blocks.iter_mut().zip(refs) {
        if responses && block["type"] == "message" {
            if let Some(parts) = block["content"].as_array_mut() {
                let ranges = reference["text"]
                    .as_array()
                    .ok_or_else(|| anyhow!("missing replay text ranges"))?;
                ensure!(ranges.len() == parts.len(), "invalid replay text ranges");
                for (part, range) in parts.iter_mut().zip(ranges) {
                    if !range.is_null() {
                        ensure!(part.is_object(), "invalid replay text block");
                        part["text"] = json!(slice(text, range)?);
                    }
                }
            }
        } else if !responses && block["type"] == "text" {
            ensure!(block.is_object(), "invalid replay text block");
            block["text"] = json!(slice(text, &reference["text"])?);
        }
        if let Some(index) = reference["call"].as_u64() {
            let call = calls
                .get(index as usize)
                .ok_or_else(|| anyhow!("missing replay tool call"))?;
            ensure!(block.is_object(), "invalid replay tool block");
            block[if responses { "call_id" } else { "id" }] = json!(call.id);
            block["name"] = json!(call.name);
            if responses {
                if let Some(value) = reference.get("arguments_verbatim") {
                    block["arguments"] = value.clone();
                } else if reference["arguments_absent"] != true {
                    let canonical = call.arguments.to_string();
                    block["arguments"] = json!(if reference["arguments"].is_array() {
                        render_json_layout(&reference["arguments"], &canonical)?
                    } else {
                        canonical
                    });
                }
            } else {
                block["input"] = call.arguments.clone();
            }
        }
    }
    Ok(blocks)
}

fn slice<'a>(text: &'a str, range: &Value) -> Result<&'a str> {
    let start = range[0]
        .as_u64()
        .ok_or_else(|| anyhow!("invalid replay text start"))? as usize;
    let end = range[1]
        .as_u64()
        .ok_or_else(|| anyhow!("invalid replay text end"))? as usize;
    text.get(start..end)
        .ok_or_else(|| anyhow!("invalid replay text boundary"))
}

// Keep only lexical differences (whitespace, key order, alternate escapes) from
// the parsed JSON in ToolCall. Common canonical arguments need no layout at all.
fn json_tokens(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        if bytes[index] == b'"' {
            index += 1;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' => index += 2,
                    b'"' => {
                        index += 1;
                        break;
                    }
                    _ => index += 1,
                }
            }
        } else if bytes[index].is_ascii_whitespace() || b"{}[]:,".contains(&bytes[index]) {
            index += 1;
            continue;
        } else {
            while index < bytes.len()
                && !bytes[index].is_ascii_whitespace()
                && !b"{}[]:,".contains(&bytes[index])
            {
                index += 1;
            }
        }
        tokens.push(&text[start..index.min(bytes.len())]);
    }
    tokens
}

fn json_layout(raw: &str, canonical: &str) -> Value {
    let tokens = json_tokens(canonical);
    let mut layout = Vec::new();
    let mut offset = 0;
    for token in json_tokens(raw) {
        let start = token.as_ptr() as usize - raw.as_ptr() as usize;
        if start > offset {
            layout.push(json!(&raw[offset..start]));
        }
        if let Some(index) = tokens.iter().position(|candidate| *candidate == token) {
            layout.push(json!(index));
        } else {
            layout.push(json!(token));
        }
        offset = start + token.len();
    }
    if offset < raw.len() {
        layout.push(json!(&raw[offset..]));
    }
    json!(layout)
}

fn render_json_layout(layout: &Value, canonical: &str) -> Result<String> {
    let tokens = json_tokens(canonical);
    let mut result = String::new();
    for part in layout
        .as_array()
        .ok_or_else(|| anyhow!("invalid argument layout"))?
    {
        if let Some(index) = part.as_u64() {
            result.push_str(
                tokens
                    .get(index as usize)
                    .ok_or_else(|| anyhow!("invalid argument token reference"))?,
            );
        } else {
            result.push_str(
                part.as_str()
                    .ok_or_else(|| anyhow!("invalid argument layout part"))?,
            );
        }
    }
    Ok(result)
}

/// Projected arguments deliberately differ from the archive; discard its lexical layout.
pub(crate) fn clear_argument_layout(state: &mut Value, index: usize) {
    if state["format"] == FORMAT
        && let Some(refs) = state["refs"].as_array_mut()
    {
        for reference in refs {
            if reference["call"].as_u64() == Some(index as u64) {
                reference.as_object_mut().unwrap().remove("arguments");
                reference
                    .as_object_mut()
                    .unwrap()
                    .remove("arguments_absent");
                reference
                    .as_object_mut()
                    .unwrap()
                    .remove("arguments_verbatim");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: "call-id".into(),
            name: "read".into(),
            arguments: json!({"a": [true, 1, "π"], "z": "large-value"}),
        }
    }

    #[test]
    fn responses_replay_is_byte_exact_with_mixed_items_and_json_formatting() {
        let raw = "{ \n \"z\" : \"large-value\", \"a\" : [true,1,\"π\"] }";
        let items = json!([
            {"type":"reasoning","id":"opaque-r","encrypted_content":"signed+/==","summary":[{"type":"summary_text","text":"plan"}]},
            {"type":"message","id":"opaque-m","role":"assistant","status":"completed","content":[{"type":"output_text","text":"First π","annotations":[{"opaque":true}]}]},
            {"type":"function_call","id":"opaque-f","call_id":"call-id","name":"read","arguments":raw,"status":"completed"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":" then"},{"type":"output_text","text":" last"}]}
        ]);
        let text = "First π then last";
        let calls = [call()];
        let state = slim(json!({"adapter":"responses","items":items}), text, &calls).unwrap();
        let stored = state.to_string();
        assert!(!stored.contains("First π"));
        assert!(!stored.contains("large-value"));
        assert!(!stored.contains("call-id"));
        assert_eq!(
            serde_json::to_vec(&hydrate(&state, text, &calls).unwrap()).unwrap(),
            serde_json::to_vec(&items).unwrap()
        );
    }

    #[test]
    fn messages_replay_preserves_interleaved_text_calls_and_signed_thinking() {
        let blocks = json!([
            {"type":"thinking","thinking":"plan","signature":"sig+/==","extra":{"keep":true}},
            {"type":"text","text":"α"},
            {"type":"tool_use","id":"call-id","name":"read","input":call().arguments,"extra":12},
            {"type":"redacted_thinking","data":"opaque-data"},
            {"type":"text","text":"β"}
        ]);
        let calls = [call()];
        let state = slim(
            json!({"adapter":"anthropic-messages","content":blocks}),
            "αβ",
            &calls,
        )
        .unwrap();
        assert!(!state.to_string().contains("large-value"));
        assert!(state["content"][1].get("text").is_none());
        assert_eq!(
            serde_json::to_vec(&hydrate(&state, "αβ", &calls).unwrap()).unwrap(),
            serde_json::to_vec(&blocks).unwrap()
        );
    }

    #[test]
    fn legacy_reads_keep_full_state_and_ignore_different_message_projection() {
        for (adapter, field, blocks) in [
            (
                "responses",
                "items",
                json!([{"type":"message","content":[{"type":"output_text","text":"legacy"}]}]),
            ),
            (
                "anthropic-messages",
                "content",
                json!([{"type":"text","text":"legacy"}]),
            ),
        ] {
            let mut state = json!({"adapter":adapter});
            state[field] = blocks.clone();
            let before = state.clone();
            assert_eq!(json!(hydrate(&state, "projection", &[]).unwrap()), blocks);
            assert_eq!(state, before);
        }
    }

    #[test]
    fn argument_projection_discards_original_layout() {
        let calls = [call()];
        let mut state = slim(json!({"adapter":"responses","items":[{"type":"function_call","call_id":"call-id","name":"read","arguments":"{ \"z\": \"large-value\", \"a\": [true,1,\"π\"] }"}]}), "", &calls).unwrap();
        let mut projected = call();
        projected.arguments = json!({"preview":"clipped"});
        clear_argument_layout(&mut state, 0);
        assert_eq!(
            hydrate(&state, "", &[projected]).unwrap()[0]["arguments"],
            "{\"preview\":\"clipped\"}"
        );
    }

    #[test]
    fn alternate_escapes_duplicate_keys_and_numbers_replay_exactly() {
        for raw in [
            r#"{ "x": "\u03c0", "n": 1.0e+1 }"#,
            r#"{"x":"old","x":"new"}"#,
            "null",
            "{}",
        ] {
            let call = ToolCall {
                id: "c".into(),
                name: "f".into(),
                arguments: serde_json::from_str(raw).unwrap(),
            };
            let state = slim(json!({"adapter":"responses","items":[{"type":"function_call","call_id":"c","name":"f","arguments":raw}]}), "", std::slice::from_ref(&call)).unwrap();
            assert_eq!(hydrate(&state, "", &[call]).unwrap()[0]["arguments"], raw);
        }
    }
}
