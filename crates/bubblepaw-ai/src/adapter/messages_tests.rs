//! Unit tests of the adapter.
use super::*;
use crate::{Message, ToolDefinition};

fn message(role: Role, content: &str) -> Message {
    Message {
        id: "fixture-message".into(),
        session_id: "fixture-session".into(),
        role,
        origin: None,
        content: crate::text_blocks(content),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        created_at: String::new(),
        ..Default::default()
    }
}

fn tool_turn() -> Message {
    let mut assistant = message(Role::Assistant, "Reading the synthetic path.");
    assistant.tool_calls = vec![ToolCall {
        id: "call-guard".into(),
        name: "read".into(),
        arguments: json!({"path":"synthetic"}),
    }];
    assistant
}

fn tool_result() -> Message {
    let mut result = message(Role::Tool, "Synthetic result.");
    result.tool_call_id = Some("call-guard".into());
    result
}

fn tool_state(assistant: &Message) -> Value {
    let call = &assistant.tool_calls[0];
    json!({
        "adapter":"anthropic-messages",
        "content":[
            {"type":"text","text":assistant.text()},
            {"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}
        ],
        "stop_reason":"tool_use"
    })
}

fn loop_request(assistant: Message) -> ModelRequest {
    ModelRequest {
        model: "claude-haiku-5-5".into(),
        messages: vec![
            message(Role::User, "Read the synthetic path."),
            assistant,
            tool_result(),
        ],
        ..Default::default()
    }
}

#[test]
fn messages_projection_shares_native_guards_and_omits_only_opaque_payloads() -> Result<()> {
    let provider = std::sync::Arc::new(MessagesProvider::api_key(
        "synthetic-key",
        "http://127.0.0.1:1",
    )?);
    let mut assistant = message(Role::Assistant, "done");
    assistant.provider_state = Some(crate::with_provider_state_origin(
        json!({"adapter":"anthropic-messages","content":[
            {"type":"thinking","thinking":"readable thought","signature":"opaque signature".repeat(10_000)},
            {"type":"redacted_thinking","data":"opaque redaction".repeat(10_000)},
            {"type":"thinking","thinking":"withheld thought","signature":"", "unsigned_cut":true},
            {"type":"text","text":"done"}
        ]}),
        "anthropic-messages",
        Some("test"),
        "native",
    )?);
    let mut request = ModelRequest {
        model: "test".into(),
        system_prompt: "stable instructions".into(),
        messages: vec![
            message(Role::User, "task"),
            assistant,
            message(Role::User, "next"),
        ],
        disabled_tools: vec![read_tool("read description", json!({"type":"object"}))],
        ..Default::default()
    };
    let original = serde_json::to_value(&request)?;
    let original_body = MessagesProvider::body(&request)?;
    let input = MessagesProvider::token_input(&request)?;
    let rendered = input.to_string();
    assert!(rendered.contains("readable thought"));
    assert!(rendered.contains("read description"));
    assert!(rendered.contains("stable instructions"));
    assert!(!rendered.contains("opaque signature"));
    assert!(!rendered.contains("opaque redaction"));
    assert!(!rendered.contains("withheld thought"));
    assert!(input.get("model").is_none());
    assert!(input.get("max_tokens").is_none());
    assert_eq!(
        provider.estimate_input_tokens(&request, &|value| {
            assert_eq!(value, &input);
            123
        })?,
        Some(123)
    );
    assert_eq!(serde_json::to_value(&request)?, original);
    assert_eq!(MessagesProvider::body(&request)?, original_body);

    request.model = "different-model".into();
    assert!(
        !MessagesProvider::token_input(&request)?
            .to_string()
            .contains("readable thought")
    );
    let invalid = loop_request(tool_turn());
    let estimate_error = provider
        .estimate_input_tokens(&invalid, &|_| 0)
        .unwrap_err();
    assert_eq!(
        estimate_error.to_string(),
        MessagesProvider::body(&invalid).unwrap_err().to_string()
    );
    Ok(())
}

#[test]
fn thinking_tool_loop_rejects_missing_replay_state() {
    let error = MessagesProvider::body(&loop_request(tool_turn())).unwrap_err();
    assert!(error.to_string().contains("current thinking tool loop"));
    assert!(error.to_string().contains("fixture-message"));
    assert!(error.to_string().contains("missing provider_state"));
}

#[test]
fn thinking_tool_loop_rejects_missing_state_after_generated_host_notice() {
    let mut request = loop_request(tool_turn());
    let mut notice = message(Role::User, "[host notice]\nSynthetic job completed.");
    notice.origin = Some(crate::MessageOrigin::Generated);
    request.messages.push(notice);
    let error = MessagesProvider::body(&request).unwrap_err();
    assert!(error.to_string().contains("current thinking tool loop"));
    assert!(error.to_string().contains("missing provider_state"));
}

#[test]
fn thinking_tool_loop_rejects_incompatible_replay_state() -> Result<()> {
    for (kind, model) in [
        ("responses", "claude-haiku-5-5"),
        ("anthropic-messages", "other-model"),
    ] {
        let mut assistant = tool_turn();
        assistant.provider_state = Some(crate::with_provider_state_origin(
            tool_state(&assistant),
            kind,
            Some(model),
            "synthetic",
        )?);
        let error = MessagesProvider::body(&loop_request(assistant)).unwrap_err();
        assert!(error.to_string().contains("compatible replay state"));
        assert!(
            error
                .to_string()
                .contains("incompatible provider_state adapter/model")
        );
    }
    let mut assistant = tool_turn();
    assistant.provider_state = Some(json!({"adapter":"responses","items":[]}));
    assert!(MessagesProvider::body(&loop_request(assistant)).is_err());
    Ok(())
}

#[test]
fn thinking_tool_loop_rejects_foreign_reasoning_only_record() -> Result<()> {
    let mut assistant = tool_turn();
    assistant.provider_state = Some(tool_state(&assistant));
    let mut request = loop_request(assistant);
    let mut reasoning = message(Role::Assistant, "");
    reasoning.id = "reasoning-guard".into();
    reasoning.provider_state = Some(crate::with_provider_state_origin(
        json!({"adapter":"anthropic-messages","content":[
            {"type":"thinking","thinking":"Synthetic plan.","signature":"sig+/==猫"}
        ]}),
        "anthropic-messages",
        Some("other-model"),
        "synthetic",
    )?);
    request.messages.insert(1, reasoning);
    let error = MessagesProvider::body(&request).unwrap_err();
    assert!(error.to_string().contains("reasoning-guard"));
    Ok(())
}

#[test]
fn thinking_tool_loop_checks_every_current_call_turn() {
    let missing = tool_turn();
    let mut valid = tool_turn();
    valid.provider_state = Some(tool_state(&valid));
    let mut request = loop_request(missing);
    request.messages.extend([valid, tool_result()]);
    assert!(MessagesProvider::body(&request).is_err());
}

#[test]
fn thinking_tool_loop_allows_fallback_after_completed_answer_or_new_user() -> Result<()> {
    for state in [
        None,
        Some(json!({"adapter":"responses","items":[
            {"type":"reasoning","encrypted_content":"foreign-opaque"}
        ]})),
    ] {
        for completed_answer in [false, true] {
            let mut assistant = tool_turn();
            assistant.provider_state = state.clone();
            let mut request = loop_request(assistant.clone());
            if completed_answer {
                request.messages.push(message(Role::Assistant, "Finished."));
                let mut current = tool_turn();
                current.provider_state = Some(tool_state(&current));
                request.messages.extend([current, tool_result()]);
            } else {
                request.messages.push(message(Role::User, "Next task."));
            }
            let body = MessagesProvider::body(&request)?;
            let expected = tool_state(&assistant)["content"].clone();
            assert_eq!(body["messages"][1]["content"], expected);
            assert!(!body.to_string().contains("foreign-opaque"));
        }
    }
    Ok(())
}

#[test]
fn thinking_tool_loop_accepts_native_reply_without_thinking() -> Result<()> {
    let assistant = tool_turn();
    let mut request = loop_request(assistant.clone());
    request.tools = vec![read_tool("Read synthetic data", json!({"type":"object"}))];
    let blocks = tool_state(&assistant)["content"].clone();
    let response = MessagesProvider::parse(
        json!({"stop_reason":"tool_use","content":blocks,"usage":{}}),
        &request,
    )?;
    let state = super::super::replay_state::slim(
        response.provider_state.unwrap(),
        &response.text,
        &response.tool_calls,
    )?;
    request.messages[1].provider_state = Some(crate::with_provider_state_origin(
        state,
        "anthropic-messages",
        Some(&request.model),
        "native",
    )?);
    assert_eq!(
        MessagesProvider::body(&request)?["messages"][1]["content"],
        blocks
    );
    Ok(())
}

#[test]
fn thinking_tool_loop_rejects_malformed_compatible_envelope() {
    let mut assistant = tool_turn();
    assistant.provider_state = Some(json!({"adapter":"anthropic-messages"}));
    let error = MessagesProvider::body(&loop_request(assistant)).unwrap_err();
    assert!(error.to_string().contains("missing replay blocks"));
}

#[test]
fn complete_request_body_is_byte_exact_for_legacy_and_slim_signed_history() -> Result<()> {
    let calls = vec![
        ToolCall {
            id: "call-a".into(),
            name: "read".into(),
            arguments: json!({"path":"ARGUMENT_SENTINEL_A"}),
        },
        ToolCall {
            id: "call-b".into(),
            name: "read".into(),
            arguments: json!({"path":"ARGUMENT_SENTINEL_B"}),
        },
    ];
    let fragments = [
        "FIRST_CANONICAL_TEXT π",
        "SECOND_CANONICAL_TEXT",
        "THIRD_CANONICAL_TEXT",
    ];
    let content = json!([
        {"type":"thinking","thinking":"Synthetic plan.","signature":"sig+/==","opaque":{"keep":true}},
        {"type":"text","text":fragments[0],"extra":"retained"},
        {"type":"tool_use","id":calls[0].id,"name":calls[0].name,"input":calls[0].arguments},
        {"type":"redacted_thinking","data":"synthetic-redacted+/=="},
        {"type":"text","text":fragments[1]},
        {"type":"tool_use","id":calls[1].id,"name":calls[1].name,"input":calls[1].arguments},
        {"type":"text","text":fragments[2]}
    ]);
    let full_state =
        json!({"adapter":"anthropic-messages","content":content,"stop_reason":"tool_use"});
    let mut assistant = message(Role::Assistant, &fragments.concat());
    assistant.tool_calls = calls;
    assistant.provider_state = Some(full_state.clone());
    let mut result_a = message(Role::Tool, "Synthetic first result.");
    result_a.tool_call_id = Some("call-a".into());
    let mut result_b = message(Role::Tool, "Synthetic second error.");
    result_b.tool_call_id = Some("call-b".into());
    result_b.is_error = true;
    let legacy = ModelRequest {
        model: "claude-haiku-5-5".into(),
        system_prompt: "Synthetic instructions.".into(),
        messages: vec![
            message(Role::System, "Additional guidance."),
            message(Role::User, "Read both synthetic paths."),
            assistant,
            result_a,
            result_b,
        ],
        tools: vec![read_tool(
            "Read a path",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        )],
        max_output_tokens: Some(128),
        ..Default::default()
    };
    let mut slim = legacy.clone();
    let assistant = &mut slim.messages[2];
    let state =
        super::super::replay_state::slim(full_state, &assistant.text(), &assistant.tool_calls)?;
    let serialized = state.to_string();
    for canonical in fragments.into_iter().chain([
        "ARGUMENT_SENTINEL_A",
        "ARGUMENT_SENTINEL_B",
        "call-a",
        "call-b",
    ]) {
        assert!(!serialized.contains(canonical));
    }
    assistant.provider_state = Some(crate::with_provider_state_origin(
        state,
        "anthropic-messages",
        Some(&slim.model),
        "native",
    )?);
    assert_eq!(
        serde_json::to_vec(&MessagesProvider::body(&legacy)?)?,
        serde_json::to_vec(&MessagesProvider::body(&slim)?)?
    );
    Ok(())
}

fn read_tool(description: &str, input_schema: serde_json::Value) -> ToolDefinition {
    serde_json::from_value(json!({
        "name": "read",
        "description": description,
        "input_schema": input_schema,
        "read_only": true,
    }))
    .unwrap()
}
