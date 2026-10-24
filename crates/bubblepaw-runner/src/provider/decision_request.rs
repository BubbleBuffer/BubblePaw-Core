//! Pure LLM decision request assembly, also used for legacy wire accounting.
use super::LlmDecisions;
use anyhow::Result;
use bubblepaw_core::{DecisionRequest, Message, ModelRequest, Role};

impl LlmDecisions {
    pub(super) fn context_kind(request: &DecisionRequest) -> String {
        let names: Vec<_> = request
            .questions
            .iter()
            .map(|question| question.name.as_str())
            .collect();
        match names.as_slice() {
            [name] => format!("decider:{name}"),
            _ => format!(
                "decider:{}",
                serde_json::to_string(&names).expect("question names are JSON")
            ),
        }
    }

    /// The chat request a decision sends; also hashed for prompt-cache accounting.
    pub(crate) fn model_request(request: &DecisionRequest) -> Result<ModelRequest> {
        Ok(ModelRequest {
            model: request.model.clone(),
            system_prompt: Self::prompt(request)?,
            messages: vec![Message {
                id: String::new(),
                session_id: request.session_id.clone().unwrap_or_default(),
                role: Role::User,
                origin: None,
                content: request.input.clone(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                provider_state: None,
                is_error: false,
                created_at: String::new(),
            }],
            tools: Vec::new(),
            disabled_tools: Vec::new(),
            max_output_tokens: None,
            reasoning_effort: Some(request.reasoning_effort),
            output_schema: None,
            session_id: String::new(),
            ..Default::default()
        })
    }
}
