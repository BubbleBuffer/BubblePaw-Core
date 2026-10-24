//! Responses request serialization.
use super::*;

impl ResponsesProvider {
    pub(super) fn body(&self, request: &ModelRequest) -> Result<Value> {
        ensure!(!request.model.is_empty(), "model must be selected");
        let mut input = Vec::new();
        for message in &request.messages {
            match message.role{
                Role::Tool=> {
                    let (text, images) = crate::images::split_wire(&message.content);
                    let output = if images.is_empty() { json!(text) } else {
                        let mut parts = vec![json!({"type":"input_text","text":text})];
                        parts.extend(images.into_iter().map(|image| json!({"type":"input_image","image_url":format!("data:{};base64,{}",image["mime_type"].as_str().unwrap_or(""),image["data"].as_str().unwrap_or(""))})));
                        json!(parts)
                    };
                    input.push(json!({"type":"function_call_output","call_id":message.tool_call_id.as_ref().ok_or_else(||anyhow!("tool message needs call_id"))?,"output":output}));
                },
                Role::Assistant=>{
                    if let Some(state)=&message.provider_state
                        && bubblepaw_core::provider_state_replay_compatible(state, "responses", &request.model)
                        && state["items"].is_array() {
                        input.extend(replay_state::hydrate(state, &message.content, &message.tool_calls)?);continue;
                    }
                    if !message.content.is_empty(){input.push(json!({"role":"assistant","content":[{"type":"output_text","text":message.content}]}));}
                    for call in &message.tool_calls{input.push(json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":serde_json::to_string(&call.arguments)?}));}
                }
                Role::System|Role::User=>input.push(json!({"role":if message.role==Role::System{"system"}else{"user"},"content":[{"type":"input_text","text":message.content}]}))
            }
        }
        let mut body = json!({"model":request.model,"instructions":request.system_prompt,"input":input,"store":false,"stream":true,"include":["reasoning.encrypted_content"]});
        let configured_effort = self
            .reasoning_effort
            .as_deref()
            .map(parse_reasoning_effort)
            .transpose()?;
        let effort = request
            .reasoning_effort
            .or(configured_effort)
            .unwrap_or_default();
        body["reasoning"] = json!({"effort":effort.as_str()});
        if matches!(self.credentials, Credentials::ApiKey(_))
            && let Some(limit) = request.max_output_tokens
        {
            body["max_output_tokens"] = json!(limit);
        }
        // Native tools stay native. Responses non-strict function calling accepts
        // optional fields without altering the semantic tool schema or exposure.
        tool_wire::insert_tools(&mut body, request, tool_wire::responses_declaration);
        Ok(body)
    }
}
