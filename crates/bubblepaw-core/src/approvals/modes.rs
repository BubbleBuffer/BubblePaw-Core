use crate::{ToolCall, ToolContext};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

#[path = "prompt.rs"]
mod prompt;
pub use prompt::DEFAULT_AUTO_PROMPT;

/// What a tool declares about itself.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    #[default]
    None,
    Confirm,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Ask,
    Auto,
    Allow,
    Deny,
}

/// A host-resolved approver model or role. Registry routing stays outside the
/// core. The gate sends the rendered `prompt` once; a yes answer approves.
#[derive(Clone)]
pub struct AutoApprovalConfig {
    pub provider: Arc<dyn crate::DecisionProvider>,
    pub model: String,
    /// Template with `{user_request}`, `{tool}`, `{description}`, `{arguments}`
    /// and `{working_directory}`; see [`crate::DEFAULT_AUTO_PROMPT`].
    pub prompt: String,
    pub working_directory: String,
    /// Resolve host model settings once at each automatic approval request.
    pub resolve: Option<Arc<dyn Fn() -> AutoApprovalConfig + Send + Sync>>,
}

#[derive(Clone)]
pub(crate) struct AutoApprovalContext {
    pub config: AutoApprovalConfig,
    pub user_request: String,
}

pub(super) const AUTO_ARGUMENT_BYTES: usize = 4096;
const AUTO_QUESTION: &str = "approve";

pub(super) fn bounded_arguments(arguments: &Value) -> String {
    let mut text = arguments.to_string();
    if text.len() > AUTO_ARGUMENT_BYTES {
        let mut end = AUTO_ARGUMENT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[arguments truncated]");
    }
    text
}

/// Runs `auto_reason` for the gate. An error is logged and falls through to a
/// person's `ask`, so a missing or failing approver stays visible.
pub(super) async fn auto_reason_logged(
    store: &crate::Store,
    auto: &AutoApprovalContext,
    context: &ToolContext,
    call: &ToolCall,
    definition: &crate::ToolDefinition,
) -> Option<String> {
    match auto_reason(store, auto, context, call, definition).await {
        Ok(reason) => reason,
        Err(error) => {
            tracing::warn!("automatic approval failed; asking the person instead: {error:#}");
            None
        }
    }
}

pub(super) async fn auto_reason(
    store: &crate::Store,
    auto: &AutoApprovalContext,
    context: &ToolContext,
    call: &ToolCall,
    definition: &crate::ToolDefinition,
) -> Result<Option<String>> {
    let resolved = auto.config.resolve.as_ref().map(|resolve| resolve());
    let config = resolved.as_ref().unwrap_or(&auto.config);
    let arguments = bounded_arguments(&call.arguments);
    let input = prompt::render(
        &config.prompt,
        &prompt::PromptFields {
            user_request: &auto.user_request,
            tool: &call.name,
            description: &definition.description,
            arguments: &arguments,
            working_directory: &config.working_directory,
        },
    );
    let request = crate::DecisionRequest {
        model: config.model.clone(),
        input,
        questions: vec![crate::Question {
            name: AUTO_QUESTION.into(),
            instructions: "Yes if the answer to the question in the input is yes.".into(),
            kind: crate::QuestionKind::Predicate,
        }],
        session_id: Some(context.session_id.clone()),
        ..Default::default()
    };
    let (provider, model) = config
        .model
        .split_once('/')
        .unwrap_or(("unknown", &config.model));
    let mut fallback = crate::RequestUsage::new(provider, model);
    fallback.session_id.clone_from(&context.session_id);
    fallback.run_id = Some(context.run_id.clone());
    fallback.kind = crate::RequestKind::Decider;
    let (response, mut records) = crate::capture_request_usage_with_store(
        store, &context.session_id, Some(&context.run_id), crate::RequestKind::Decider,
        async { tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => Err(anyhow::anyhow!("Cancelled during automatic approval")),
            response = config.provider.decide(request, context.cancellation.clone()) => response,
        } },
    ).await;
    if records.is_empty() {
        match &response {
            Ok(response) => {
                fallback.usage = response.usage.clone().into();
                if !response.model.is_empty() {
                    if let Some((provider, model)) = response.model.split_once('/') {
                        fallback.provider = provider.to_owned();
                        fallback.model = model.to_owned();
                    } else {
                        fallback.model.clone_from(&response.model);
                    }
                }
            }
            Err(error) => {
                fallback.failed = true;
                fallback.usage = crate::ProviderError::of(error)
                    .and_then(|error| error.usage.clone())
                    .unwrap_or_default();
            }
        }
        records.push(fallback);
    }
    // Both the approved and pending paths immediately insert the gate record.
    // Stage usage for that existing canonical transaction, including failures.
    for record in records {
        store.queue_request_usage(record)?;
    }
    let response = response?;
    if context.cancellation.is_cancelled() {
        return Ok(None);
    }
    let probability = response.answers.iter().find_map(|answer| {
        if answer.name == AUTO_QUESTION
            && let crate::AnswerKind::Predicate { probability } = answer.kind
        {
            Some(probability)
        } else {
            None
        }
    });
    // One answer decides: yes (p > 0.5) approves, anything else asks a person.
    Ok(probability
        .filter(|p| p.is_finite() && *p > 0.5 && *p <= 1.0)
        .map(|p| format!("auto {} p={p}", response.model)))
}

/// Approval modes and exact-name deny rules. Denies precede every allowance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ApprovalPolicy {
    pub confirm: ApprovalMode,
    pub tools: BTreeMap<String, ApprovalMode>,
    /// Exact tool names denied before any mode or session allowance.
    pub deny: Vec<String>,
    pub timeout_secs: Option<u64>,
    /// The fixed exec blocklist (blocklist.rs), checked before everything else.
    pub hardline: bool,
}
impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            confirm: ApprovalMode::Ask,
            tools: BTreeMap::new(),
            deny: Vec::new(),
            timeout_secs: None,
            hardline: true,
        }
    }
}
impl ApprovalPolicy {
    /// Deny rules first, then the mode before session allowances are consulted.
    pub fn mode(&self, tool: &str, declared: Approval) -> ApprovalMode {
        if self.denied_rule(tool).is_some() {
            return ApprovalMode::Deny;
        }
        match (self.tools.get(tool), declared) {
            (Some(mode), _) => *mode,
            (None, Approval::Confirm) => self.confirm,
            (None, Approval::None) => ApprovalMode::Allow,
        }
    }
}
