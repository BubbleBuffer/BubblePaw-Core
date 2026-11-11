use super::*;
use bubblepaw_core::{JsonSchema, ReasoningEffort};

fn output_schema() -> JsonSchema {
    JsonSchema::new(
        "answer",
        json!({
            "type":"object",
            "properties":{"answer":{"type":"string"}},
            "required":["answer"],
            "additionalProperties":false
        }),
    )
    .unwrap()
}

#[tokio::test]
async fn extension_omits_legacy_schema_and_forwards_supported_effort_without_changing_other_fields()
{
    for (capabilities, schema, effort) in [
        (
            json!({"output_schema":true,"reasoning_effort":true}),
            Some(output_schema()),
            Some(ReasoningEffort::High),
        ),
        (json!({"output_schema":true}), Some(output_schema()), None),
        (
            json!({"reasoning_effort":true}),
            None,
            Some(ReasoningEffort::Low),
        ),
    ] {
        let provider = fake_with_capabilities(Some(capabilities));
        let mut request = ext_step("direct-model", "hello", "e1", Some("m0"));
        request.system_prompt = "system".into();
        request.max_output_tokens = Some(123);
        request.output_schema = schema;
        request.reasoning_effort = effort;
        let mut expected = serde_json::to_value(&request).unwrap();
        expected.as_object_mut().unwrap().remove("output_schema");
        let state = call(&provider, request)
            .await
            .unwrap()
            .provider_state
            .unwrap();
        assert_eq!(state["request"], expected);
        assert_eq!(state["step_count"], 1);
    }
}

#[tokio::test]
async fn extension_omits_unset_schema_and_effort_and_preserves_legacy_step_fields() {
    let provider = fake();
    let request = ext_step("direct-model", "hello", "e1", Some("m0"));
    let expected = serde_json::to_value(&request).unwrap();
    let state = call(&provider, request)
        .await
        .unwrap()
        .provider_state
        .unwrap();
    let wire = &state["request"];
    assert!(wire.get("output_schema").is_none());
    assert!(wire.get("reasoning_effort").is_none());
    // Other optional fields keep their existing null serialization.
    assert_eq!(wire.get("max_output_tokens"), Some(&Value::Null));
    assert_eq!(*wire, expected);
}

async fn assert_unsupported_option_is_not_sent(
    capabilities: Option<Value>,
    model: &str,
    option: &str,
) {
    let provider = fake_with_capabilities(capabilities);
    let mut request = ext_step(model, "hello", "e1", None);
    match option {
        "reasoning_effort" => request.reasoning_effort = Some(ReasoningEffort::High),
        _ => unreachable!(),
    }
    let error = call(&provider, request).await.unwrap_err();
    let typed = ProviderError::of(&error).expect("typed unsupported-option error");
    assert_eq!(typed.kind, ProviderErrorKind::Fatal);
    assert_eq!(
        typed.message,
        format!("extension fake does not support {option}")
    );
    let state = call(&provider, ext_step("direct-model", "hello", "e1", None))
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert_eq!(state["step_count"], 1, "rejected request reached step");
}

#[tokio::test]
async fn extension_ignores_legacy_schema_regardless_of_support_flag() {
    for capabilities in [
        None,
        Some(json!({})),
        Some(json!({"output_schema":false})),
        Some(json!({"output_schema":true})),
    ] {
        let provider = fake_with_capabilities(capabilities);
        let mut request = ext_step("unknown-model", "hello", "e1", None);
        request.output_schema = Some(output_schema());
        let state = call(&provider, request)
            .await
            .unwrap()
            .provider_state
            .unwrap();
        assert!(state["request"].get("output_schema").is_none());
        assert_eq!(state["step_count"], 1);
    }
}

#[tokio::test]
async fn extension_rejects_effort_when_its_support_flag_is_absent_or_false() {
    for capabilities in [
        None,
        Some(json!({})),
        Some(json!({"output_schema":true})),
        Some(json!({"reasoning_effort":false,"output_schema":true})),
    ] {
        assert_unsupported_option_is_not_sent(capabilities, "direct-model", "reasoning_effort")
            .await;
    }
}

#[tokio::test]
async fn extension_unknown_model_cannot_bypass_unsupported_option_checks() {
    assert_unsupported_option_is_not_sent(
        Some(json!({"output_schema":true})),
        "unknown-model",
        "reasoning_effort",
    )
    .await;
}

#[tokio::test]
async fn extension_omits_invalid_legacy_schema_without_validating_it() {
    let provider = fake();
    let mut request = ext_step("direct-model", "hello", "e1", None);
    request.output_schema = Some(JsonSchema {
        name: "invalid name".into(),
        schema: json!({"type":"object"}),
    });
    let state = call(&provider, request)
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert!(state["request"].get("output_schema").is_none());
    assert_eq!(state["step_count"], 1);
}

#[tokio::test]
async fn extension_forwards_reserved_submission_with_missing_or_stale_declaration() {
    for declaration in 0..=2 {
        let provider = fake();
        let mut request = ext_step("direct-model", "call submit_result", "e1", None);
        request.tools.clear();
        if declaration > 0 {
            request.tools.push(ToolDefinition {
                name:"submit_result".into(),description:"Frozen schema".into(),
                input_schema:json!({"type":"object","properties":{"old":{"type":"string"}},"required":["old"],"additionalProperties":false}),
                read_only:true,approval:Default::default(),
            });
            if declaration == 2 {
                request.disabled_tools = std::mem::take(&mut request.tools);
            }
        }
        let frozen = serde_json::to_value(&request).unwrap();
        let result = call(&provider, request).await.unwrap();
        assert_eq!(result.tool_calls[0].name, "submit_result");
        assert_eq!(result.tool_calls[0].arguments, json!({"value":"hi"}));
        let wire = &result.provider_state.unwrap()["request"];
        assert_eq!(wire["tools"], frozen["tools"]);
        assert_eq!(wire["disabled_tools"], frozen["disabled_tools"]);
    }
}

#[tokio::test]
async fn extension_rejects_other_undeclared_names_and_retains_reported_usage() {
    let provider = fake();
    let mut request = ext_step("direct-model", "call unknown", "e1", None);
    request.tools.clear();
    let error = call(&provider, request).await.unwrap_err();
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.kind, ProviderErrorKind::Fatal);
    assert!(typed.message.contains("unavailable tool"));
    assert_eq!(typed.usage.as_ref().unwrap().input_tokens, 5);
}
