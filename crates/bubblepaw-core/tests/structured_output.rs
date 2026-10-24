use bubblepaw_core::{JsonSchema, ModelRequest, ModelResponse, ToolCall};
use serde::Deserialize;
use serde_json::json;

#[test]
fn output_schema_is_optional_for_legacy_requests() {
    let value = serde_json::to_value(ModelRequest::default()).unwrap();
    assert!(value.get("output_schema").is_none());
    assert!(
        serde_json::from_value::<ModelRequest>(value)
            .unwrap()
            .output_schema
            .is_none()
    );
    let schema = JsonSchema::new(
        "answer",
        json!({"type":"object","additionalProperties":false}),
    )
    .unwrap();
    let request = ModelRequest {
        output_schema: Some(schema.clone()),
        ..Default::default()
    };
    let decoded: ModelRequest =
        serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
    assert!(decoded.output_schema.is_none());
    let decoded: ModelRequest = serde_json::from_value(json!({
        "model":"legacy", "system_prompt":"", "messages":[], "tools":[],
        "max_output_tokens":null, "output_schema":schema
    }))
    .unwrap();
    assert_eq!(decoded.output_schema, Some(schema));
}

#[test]
fn object_schemas_must_explicitly_close_nested_objects() {
    for schema in [
        json!({"type":"object"}),
        json!({"type":"object","additionalProperties":true}),
        json!({"type":"object","additionalProperties":{},"properties":{}}),
        json!({"type":"object","additionalProperties":false,"properties":{"child":{"type":["object","null"]}}}),
        json!({"type":"array","items":{"type":"object"}}),
        json!({"$ref":"#/$defs/child","$defs":{"child":{"type":"object"}}}),
        json!({"anyOf":[{"type":"object"}]}),
        json!({"properties":{"child":{"type":"string"}}}),
    ] {
        let error = JsonSchema::new("answer", schema).unwrap_err().to_string();
        assert!(error.contains("additionalProperties: false"), "{error}");
        assert!(error.contains("at $"), "{error}");
    }
    // JSON values in annotations are data, not nested schemas.
    JsonSchema::new(
        "answer",
        json!({
            "type":"object","additionalProperties":false,
            "properties":{"children":{"type":"array","items":{"$ref":"#/$defs/child"}}},
            "$defs":{"child":{"type":"object","additionalProperties":false}},
            "default":{"type":"object"},"examples":[{"type":"object"}]
        }),
    )
    .unwrap();
}

#[test]
fn invalid_name_and_schema_fail_clearly() {
    for name in ["", "has space", "ü", &"a".repeat(65)] {
        assert!(
            JsonSchema::new(name, json!({}))
                .unwrap_err()
                .to_string()
                .contains("name must")
        );
    }
    assert!(
        JsonSchema::new("answer", json!(true))
            .unwrap_err()
            .to_string()
            .contains("JSON schema object")
    );
    JsonSchema::new("Answer_1-v2", json!({"type":"string"})).unwrap();
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Answer {
    title: String,
    count: u32,
}

#[test]
fn typed_reply_parser_obeys_serde_and_requires_a_complete_reply() {
    let reply = ModelResponse {
        text: r#"{"title":"ready","count":2}"#.into(),
        ..Default::default()
    };
    assert_eq!(
        reply.parse_json::<Answer>().unwrap(),
        Answer {
            title: "ready".into(),
            count: 2
        }
    );
    for text in [
        "not JSON",
        r#"{"title":"ready","count":"two"}"#,
        r#"{"title":"ready","count":2,"extra":true}"#,
    ] {
        assert!(
            ModelResponse {
                text: text.into(),
                ..Default::default()
            }
            .parse_json::<Answer>()
            .is_err()
        );
    }
    let reply = ModelResponse {
        tool_calls: vec![ToolCall {
            id: "call".into(),
            name: "read".into(),
            arguments: json!({}),
        }],
        ..reply
    };
    assert!(
        reply
            .parse_json::<Answer>()
            .unwrap_err()
            .to_string()
            .contains("tool-call response")
    );
}
