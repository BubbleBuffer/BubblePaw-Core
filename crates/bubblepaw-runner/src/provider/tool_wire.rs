//! Tool declarations on the wire. A tools-off round keeps the declarations and turns the choice off.
use bubblepaw_core::{CancellationToken, ModelRequest, ModelResponse, Provider, ToolDefinition};
use serde_json::{Value, json};

/// Inserts the active tools, or the disabled set with `tool_choice` none when tools are off.
pub(super) fn insert_tools(
    body: &mut Value,
    request: &ModelRequest,
    wire: fn(&ToolDefinition) -> Value,
) {
    let tools = if request.tools.is_empty() {
        &request.disabled_tools
    } else {
        &request.tools
    };
    if tools.is_empty() {
        return;
    }
    body["tools"] = Value::Array(tools.iter().map(wire).collect());
    if request.tools.is_empty() {
        body["tool_choice"] = json!("none");
    }
}

/// Chat completions nests the declaration under `function`.
pub(super) fn chat_declaration(tool: &ToolDefinition) -> Value {
    json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}})
}

/// Responses non-strict function calling accepts optional fields without altering the schema.
pub(super) fn responses_declaration(tool: &ToolDefinition) -> Value {
    json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.input_schema,"strict":false})
}

/// The runtime owns submission validation, including runs whose frozen
/// declarations omit this reserved capability or retain an older schema.
pub(super) fn accepts_call(name: &str, declared: bool) -> bool {
    name == "submit_result" || declared
}

pub(super) fn accepts_request_call(request: &ModelRequest, name: &str) -> bool {
    accepts_call(name, request.tools.iter().any(|tool| tool.name == name))
}

/// Retain malformed argument text for a model-visible validation failure, bounded in Unicode chars.
pub(super) fn raw_arguments(raw: &str) -> Value {
    Value::String(raw.chars().take(2000).collect())
}

/// Supply object inputs for Messages replay while retaining the canonical calls.
pub(super) fn project_messages_inputs(blocks: &mut [Value]) {
    for block in blocks {
        if block["type"] == "tool_use" && !block["input"].is_object() {
            block["input"] = json!({});
        }
    }
}

/// Summary forks have their own delivery contract and do not enter the model tool loop.
pub(super) async fn generate_summary_fork(
    provider: &dyn Provider,
    request: ModelRequest,
    cancellation: CancellationToken,
) -> anyhow::Result<ModelResponse> {
    let declared: Vec<_> = request.tools.iter().map(|tool| tool.name.clone()).collect();
    let response = provider
        .generate(request, std::sync::Arc::new(|_| {}), cancellation)
        .await?;
    for call in &response.tool_calls {
        let message = if !accepts_call(&call.name, declared.contains(&call.name)) {
            "Summary model returned an unavailable tool"
        } else if !call.arguments.is_object() {
            "Summary model tool arguments must be a JSON object"
        } else {
            continue;
        };
        return Err(super::decisions::metered_model_error(
            anyhow::anyhow!(message),
            response.usage,
        ));
    }
    Ok(response)
}
