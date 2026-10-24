//! Tool declarations on the wire. A tools-off round keeps the declarations and turns the choice off.
use bubblepaw_core::{ModelRequest, ToolDefinition};
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
