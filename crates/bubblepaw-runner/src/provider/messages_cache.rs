//! Messages wire layout helpers: user-turn block order and the cache breakpoints
//! at the context tier ends (P5).
use bubblepaw_core::{CacheTiers, ModelResponse, RequestLogRow};
use serde_json::{Value, json};

/// Appends message `source`'s blocks to a wire turn. tool_result blocks stay
/// ahead of every other block in a turn, so text from a system or user message
/// never precedes a result, which Anthropic rejects.
pub(super) fn merge(
    turn: &mut Vec<Value>,
    sources: &mut Vec<usize>,
    content: Vec<Value>,
    source: usize,
) {
    for block in content {
        let at = if block["type"] == "tool_result" {
            turn.iter()
                .take_while(|block| block["type"] == "tool_result")
                .count()
        } else {
            turn.len()
        };
        turn.insert(at, block);
        sources.insert(at, source);
    }
}

/// Marks at most four ephemeral breakpoints, in prefix order: the frozen end
/// (the system block, else the last tool), the cold end, the previous request's
/// tail and the last block. Without tiers (side calls, external callers) only
/// the frozen end and the last block. Message breakpoints sit on user turns only,
/// so saved assistant blocks are never modified. `sources[turn][block]` is the
/// request message index each wire block came from.
pub(super) fn place(body: &mut Value, sources: &[Vec<usize>], tiers: Option<&CacheTiers>) {
    let frozen = if body["system"]
        .as_array()
        .is_some_and(|system| !system.is_empty())
    {
        "system"
    } else {
        "tools"
    };
    if let Some(block) = body
        .get_mut(frozen)
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
    {
        block["cache_control"] = ephemeral();
    }
    let ends = tiers.map(|tiers| [tiers.cold_end, tiers.previous_len]);
    let messages = body["messages"].as_array_mut().unwrap();
    let mut marks: Vec<_> = ends
        .into_iter()
        .flatten()
        .flatten()
        .chain([usize::MAX])
        .filter_map(|end| boundary(messages, sources, end))
        .collect();
    marks.sort_unstable();
    marks.dedup();
    for (turn, block) in marks {
        messages[turn]["content"][block]["cache_control"] = ephemeral();
    }
}

fn ephemeral() -> Value {
    json!({"type":"ephemeral"})
}

/// The last user-turn block before the first block from message `end` onward,
/// so the marked prefix holds only messages before `end`.
fn boundary(messages: &[Value], sources: &[Vec<usize>], end: usize) -> Option<(usize, usize)> {
    let mut found = None;
    for (turn, blocks) in sources.iter().enumerate() {
        for (block, &source) in blocks.iter().enumerate() {
            if source >= end {
                return found;
            }
            if messages[turn]["role"] == "user" {
                found = Some((turn, block));
            }
        }
    }
    found
}

/// Wire positions of every `cache_control` mark in the body, for the request
/// log: `system.<block>`, `tools.<index>`, `messages.<turn>.<block>`.
pub(super) fn breakpoints(body: &Value) -> Vec<String> {
    let marked = |blocks: &Value| -> Vec<usize> {
        blocks.as_array().map_or_else(Vec::new, |blocks| {
            (0..blocks.len())
                .filter(|&index| blocks[index].get("cache_control").is_some())
                .collect()
        })
    };
    let mut found = Vec::new();
    for section in ["system", "tools"] {
        found.extend(
            marked(&body[section])
                .into_iter()
                .map(|index| format!("{section}.{index}")),
        );
    }
    for (turn, message) in body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        found.extend(
            marked(&message["content"])
                .into_iter()
                .map(|block| format!("messages.{turn}.{block}")),
        );
    }
    found
}

/// Report the sent breakpoints on the response for the request log; the
/// runtime removes them before the message is stored.
pub(super) fn report(
    mut response: ModelResponse,
    breakpoints: Option<Vec<String>>,
) -> ModelResponse {
    if let Some(breakpoints) = breakpoints {
        let state = response.provider_state.get_or_insert_with(|| json!({}));
        if let Some(state) = state.as_object_mut() {
            state.insert(RequestLogRow::BREAKPOINTS_KEY.into(), json!(breakpoints));
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::{Message, ModelRequest, Role, ToolCall, ToolDefinition};

    fn message(role: Role, content: &str) -> Message {
        Message {
            id: "fixture-message".into(),
            session_id: "fixture-session".into(),
            role,
            origin: None,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        }
    }

    fn result(id: &str, text: &str) -> Message {
        let mut result = message(Role::Tool, text);
        result.tool_call_id = Some(id.into());
        result
    }

    fn tool_loop() -> Message {
        let mut assistant = message(Role::Assistant, "Reading both.");
        assistant.tool_calls = ["a", "b"]
            .map(|id| ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: json!({"path":id}),
            })
            .to_vec();
        let mut content = vec![json!({"type":"text","text":assistant.content})];
        content.extend(assistant.tool_calls.iter().map(
            |call| json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}),
        ));
        assistant.provider_state = Some(json!({
            "adapter":"anthropic-messages","content":content,"stop_reason":"tool_use"
        }));
        assistant
    }

    fn body(request: &ModelRequest) -> Value {
        super::super::MessagesProvider::body(request).unwrap()
    }

    fn request(
        system_prompt: &str,
        messages: Vec<Message>,
        tiers: Option<CacheTiers>,
    ) -> ModelRequest {
        ModelRequest {
            model: "claude-haiku-5-5".into(),
            system_prompt: system_prompt.into(),
            messages,
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
                read_only: true,
                approval: Default::default(),
            }],
            cache_tiers: tiers,
            ..Default::default()
        }
    }

    fn tiers(cold_end: Option<usize>, previous_len: Option<usize>) -> Option<CacheTiers> {
        Some(CacheTiers {
            cold_end,
            previous_len,
        })
    }

    /// Every block carrying cache_control, as `field.index[.block]`.
    fn marks(request: &ModelRequest) -> Vec<String> {
        let body = body(request);
        let mut marks = Vec::new();
        for field in ["tools", "system"] {
            for (index, block) in body[field].as_array().into_iter().flatten().enumerate() {
                if block.get("cache_control").is_some() {
                    marks.push(format!("{field}.{index}"));
                }
            }
        }
        for (turn, message) in body["messages"].as_array().unwrap().iter().enumerate() {
            for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
                if let Some(control) = block.get("cache_control") {
                    assert_eq!(control, &json!({"type":"ephemeral"}));
                    marks.push(format!("messages.{turn}.{index}"));
                }
            }
        }
        assert!(marks.len() <= 4, "{marks:?}");
        marks
    }

    #[test]
    fn no_cold_tier_marks_frozen_previous_tail_and_last_block() {
        let messages = vec![
            message(Role::User, "First question."),
            message(Role::Assistant, "First answer."),
            message(Role::User, "Second question."),
        ];
        let request = request("Static prompt.", messages, tiers(None, Some(1)));
        assert_eq!(
            marks(&request),
            ["system.0", "messages.0.0", "messages.2.0"]
        );
    }

    #[test]
    fn cold_tier_with_notes_marks_its_last_block_as_user_content() {
        let messages = vec![
            message(Role::System, "Closed chapter: retained facts."),
            message(Role::User, "Anchored task."),
            message(
                Role::System,
                "<harness kind=\"history_lookup\">Search.</harness>",
            ),
            message(Role::User, "First question."),
            message(Role::Assistant, "First answer."),
            message(Role::User, "Second question."),
        ];
        let request = request("Static prompt.", messages, tiers(Some(3), Some(4)));
        let body = body(&request);
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "Closed chapter: retained facts."
        );
        assert_eq!(
            marks(&request),
            ["system.0", "messages.0.2", "messages.0.3", "messages.2.0"]
        );
    }

    #[test]
    fn tool_results_tail_marks_the_last_result_and_the_last_tool_without_system() {
        let messages = vec![
            message(Role::User, "Read a and b."),
            tool_loop(),
            result("a", "file a"),
            result("b", "file b"),
        ];
        let request = request("", messages, tiers(None, Some(1)));
        assert_eq!(marks(&request), ["tools.0", "messages.0.0", "messages.2.1"]);
    }

    #[test]
    fn legacy_view_marks_the_stored_prefix_end() {
        let messages = vec![
            message(Role::System, "Checkpoint: earlier work."),
            message(Role::User, "Stored anchor."),
            message(Role::User, "New question."),
        ];
        let request = request("Stored view prompt.", messages, tiers(Some(2), None));
        let body = body(&request);
        assert_eq!(body["system"][0]["text"], "Stored view prompt.");
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
        assert_eq!(
            marks(&request),
            ["system.0", "messages.0.1", "messages.0.2"]
        );
    }

    #[test]
    fn without_tiers_only_the_frozen_end_and_last_block_are_marked() {
        let messages = vec![
            message(Role::System, "View block."),
            message(Role::User, "First question."),
            message(Role::Assistant, "First answer."),
            message(Role::User, "Second question."),
        ];
        let request = request("Static prompt.", messages, None);
        assert_eq!(marks(&request), ["system.0", "messages.2.0"]);
    }

    #[test]
    fn system_text_never_precedes_tool_results() -> anyhow::Result<()> {
        let messages = vec![
            message(Role::User, "Read a and b."),
            tool_loop(),
            result("a", "file a"),
            message(Role::System, "Mid-loop control."),
            result("b", "file b"),
        ];
        let body = body(&request("Static prompt.", messages, None));
        assert_eq!(
            serde_json::to_string(&body["messages"][2])?,
            serde_json::to_string(&json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"a","content":"file a","is_error":false},
                {"type":"tool_result","tool_use_id":"b","content":"file b","is_error":false},
                {"type":"text","text":"Mid-loop control.","cache_control":{"type":"ephemeral"}}
            ]}))?
        );
        Ok(())
    }

    #[test]
    fn reported_breakpoints_name_every_marked_wire_block() {
        let body = json!({
            "system": [{"type":"text","text":"a"},{"type":"text","text":"b","cache_control":{"type":"ephemeral"}}],
            "tools": [{"name":"t"}],
            "messages": [
                {"role":"user","content":[{"type":"text","text":"u","cache_control":{"type":"ephemeral"}}]},
                {"role":"assistant","content":[{"type":"text","text":"x"}]},
                {"role":"user","content":[{"type":"text","text":"v"},{"type":"text","text":"w","cache_control":{"type":"ephemeral"}}]}
            ]
        });
        let marks = breakpoints(&body);
        assert_eq!(marks, ["system.1", "messages.0.0", "messages.2.1"]);
        let mut response = report(ModelResponse::default(), Some(marks.clone()));
        assert_eq!(
            RequestLogRow::reported_breakpoints(&response),
            Some(marks.clone())
        );
        assert_eq!(RequestLogRow::take_breakpoints(&mut response), Some(marks));
        assert_eq!(response.provider_state, None, "the report is not stored");
    }
}
