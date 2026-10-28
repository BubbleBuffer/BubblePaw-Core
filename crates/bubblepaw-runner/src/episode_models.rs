//! Episode model adapters. Credentials and HTTP payloads never enter the archive or trace.
use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    AnswerKind, CancellationToken, ContextSubjectChapter, DecisionChoice, DecisionRequest, Message,
    MessageOrigin, ModelRequest, Provider, Question, QuestionKind, RelatedEvidence, RequestKind,
    Role, Usage,
};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    model_roles::{ModelRole, ModelRoles},
    provider::{LiveProviderConfig, ProviderRegistry, decision_error_usage},
    trace::JsonlTrace,
};

#[path = "episode_models_chapter.rs"]
mod chapter_requests;
pub use chapter_requests::{ChapterInput, ChapterResult};
#[path = "chapter_sections.rs"]
mod chapter_sections;
#[path = "episode_models_chat.rs"]
mod chat_requests;
#[path = "episode_models_fork.rs"]
mod fork_requests;
#[path = "episode_models_output.rs"]
mod output;
use fork_requests::summarize_fork_attempt;

#[cfg(feature = "bench")]
pub(crate) const SUMMARY_PROMPT: &str = include_str!("summary_prompt_v2.txt");
#[cfg(feature = "bench")]
pub(crate) const CHAPTER_PROMPT: &str = include_str!("chapter_prompt_v1.txt");
pub const CLASSICAL_PROMPT: &str = include_str!("classical_prompt_v1.txt");
pub(crate) const SPLIT_QUESTION: &str = "Which numbered boundary starts a different concrete subject or a meaningful new work phase compared with the last chapter? Options can be typed requests or an explicitly labeled episode summary from continuing agent work. An episode summary is not a user instruction. A phase change can occur without a new user message (for example investigation to implementation or implementation to validation). Ongoing builds, tests, Clippy runs and their intermediate results remain one validation phase; starting or finishing another check is not itself a new phase. Changing approach or correcting a failed attempt within the same concrete goal does not automatically start a new chapter. Mere progress, tool calls, clarification, corrections, approvals, complaints and continuing the same phase are not transitions. If none starts a new subject or phase, answer none.";
pub(crate) const RELATED_INSTRUCTION: &str = "Final line: \"Related: \" then each earlier chapter whose concrete goal is explicitly resumed by a current request or delegated task instruction. Use its offered ID and exact supporting words: `Related: B (request: \"resume the attachment fix\")` for typed requests, or `Related: B (task: \"continue implementing the attachment fix\")` for ChildTask instructions. A delegated task is not a human statement. The legacy `user:` label also means a typed request. Only supplied request/task instructions count; an agent mentioning the same files, project or theme is not continuity evidence. Separate entries with \"; \". Otherwise write \"Related: none\".";

#[derive(Clone)]
pub struct EpisodeModelConfig {
    pub roles: ModelRoles,
    pub retry_base_delay: Duration,
}
impl Default for EpisodeModelConfig {
    fn default() -> Self {
        Self {
            roles: ModelRoles::default(),
            retry_base_delay: Duration::from_millis(500),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SplitOption {
    pub message_id: String,
    pub user: String,
    pub agent_before: String,
}
#[derive(Clone, Debug)]
pub struct SplitInput {
    pub session_id: String,
    pub open_chapter: String,
    pub options: Vec<SplitOption>,
}
#[derive(Clone, Debug)]
pub struct SplitResult {
    pub choice: Option<String>,
    /// Supplied option order, followed by none.
    pub probabilities: Vec<f64>,
    pub model: String,
    pub usage: Usage,
    pub reasoning: Option<String>,
}
#[derive(Clone, Debug)]
pub struct SummaryInput {
    /// Extra instruction for this explicit compaction only.
    pub focus: Option<String>,
    pub session_id: String,
    pub previous_summary: Option<String>,
    pub turn_request: Option<String>,
    pub episode_text: String,
    pub cap: usize,
}
#[derive(Clone)]
pub struct EpisodeFork {
    pub request: ModelRequest,
    pub first_message_id: String,
    pub last_message_id: String,
    /// The immutable provider generation used to prepare the captured request.
    pub provider: Arc<dyn Provider>,
    pub max_request_bytes: usize,
    pub context_window: Option<usize>,
    /// Physical output reservation when the provider request has no hard limit.
    pub output_reserve_tokens: usize,
    /// False for a cold candidate captured before a main dispatch was possible.
    pub dispatched: bool,
}
#[derive(Clone, Debug)]
pub struct SummaryResult {
    pub title: String,
    pub summary: String,
    pub model: String,
    pub usage: Usage,
    pub reasoning: Option<String>,
}
#[derive(Clone, Debug)]
pub struct ClassicalInput {
    pub session_id: String,
    pub previous_summary: Option<String>,
    pub latest_user_request: Option<String>,
    pub span_text: String,
}
#[derive(Clone, Debug)]
pub struct ClassicalResult {
    pub summary: String,
    pub model: String,
    pub usage: Usage,
    pub reasoning: Option<String>,
}
#[async_trait]
pub trait EpisodeModels: Send + Sync {
    /// Stable model/config generation for automatic gather failure suppression.
    fn gather_revision(&self) -> u64 {
        0
    }
    async fn decide(&self, input: SplitInput) -> Result<SplitResult>;
    async fn summarize(&self, input: SummaryInput) -> Result<SummaryResult>;
    async fn summarize_fork(
        &self,
        _input: SummaryInput,
        _fork: EpisodeFork,
    ) -> Result<SummaryResult> {
        Err(anyhow!(
            "This episode adapter does not support full-prefix summary forks; configure summary_input=episode explicitly to use episode-only input"
        ))
    }
    /// Preserve host-owned tier annotations without changing the public fork payload.
    fn record_fork_context(&self, _session: &str, _request: &ModelRequest, _cold: &[usize]) {}
    fn record_fork_fallback(&self, _session: &str, _reason: &str) {}
    fn record_pressure_gather(&self, _session: &str, _force: bool) {}
    async fn close_chapter(&self, input: ChapterInput) -> Result<ChapterResult>;
    /// Close a chapter as a fork of the main prefix. Adapters without forks close
    /// it fresh.
    async fn close_chapter_fork(
        &self,
        input: ChapterInput,
        _fork: EpisodeFork,
    ) -> Result<ChapterResult> {
        self.close_chapter(input).await
    }
    /// Best-effort diagnostic only; failure to write observations must not change
    /// canonical coverage or turn a successful model result into a failure.
    fn record_drain_failure(&self, _session: &str, _error: &anyhow::Error) {}
    /// Best-effort diagnostic before a timed-out shutdown aborts gathering.
    fn record_shutdown_abort(
        &self,
        _aborted_calls: usize,
        _completed_results_uncommitted: usize,
        _drain_timeout: std::time::Duration,
    ) {
    }
    async fn summarize_classical(&self, _input: ClassicalInput) -> Result<ClassicalResult> {
        Err(anyhow!(
            "Classical summarization is not implemented by this model adapter"
        ))
    }
}

pub struct HttpEpisodeModels {
    config: EpisodeModelConfig,
    episode_config: crate::episodes::EpisodeConfig,
    registry: Arc<ProviderRegistry>,
    roles: ModelRoles,
    live: Option<Arc<LiveProviderConfig>>,
    trace: Option<JsonlTrace>,
    context_route: crate::context_routes::ContextRoute,
}
struct ChatResult {
    text: String,
    model: String,
    usage: Usage,
    reasoning: Option<String>,
}
#[derive(Debug)]
struct InvalidMemoryOutput(String);
impl std::fmt::Display for InvalidMemoryOutput {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "invalid episode memory: {}", self.0)
    }
}
impl std::error::Error for InvalidMemoryOutput {}
impl HttpEpisodeModels {
    pub fn from_registry(
        config: EpisodeModelConfig,
        registry: Arc<ProviderRegistry>,
        trace: Option<JsonlTrace>,
    ) -> Result<Self> {
        config.roles.validate()?;
        let roles = config.roles.clone();
        if let Some(trace) = &trace {
            registry.set_context_observation(Some(trace.observation()));
        }
        let context_route = registry.context_route();
        Ok(Self {
            config,
            episode_config: crate::episodes::EpisodeConfig::default(),
            registry,
            roles,
            live: None,
            context_route,
            trace,
        })
    }
    /// Resolve role models and their transport together for each new request.
    pub fn from_live(
        config: EpisodeModelConfig,
        live: Arc<LiveProviderConfig>,
        trace: Option<JsonlTrace>,
    ) -> Result<Self> {
        let snapshot = live.snapshot();
        let mut models = Self::from_registry(config, snapshot.registry.clone(), trace)?;
        models.live = Some(live);
        Ok(models)
    }
    pub fn with_episode_config(mut self, config: crate::episodes::EpisodeConfig) -> Result<Self> {
        config.validate()?;
        self.episode_config = config;
        Ok(self)
    }
    fn current_models(&self) -> (ModelRoles, Arc<ProviderRegistry>) {
        match &self.live {
            Some(live) => {
                let snapshot = live.snapshot();
                (snapshot.settings.roles.clone(), snapshot.registry.clone())
            }
            None => (self.roles.clone(), self.registry.clone()),
        }
    }
    fn trace_call(&self, kind: &str, model: &str, usage: &Usage, started: Instant, outcome: &str) {
        trace_call(
            self.trace.as_ref(),
            kind,
            model,
            usage,
            started,
            outcome,
            None,
        );
    }
    fn fork_unavailable(&self, session: &str, model: &str, reason: &str) -> anyhow::Error {
        if let Some(trace) = &self.trace {
            trace.runtime_info(
                json!({"event":"episode_fork_unavailable", "session_id":session,
                "model":model,"mode":"fork","reason":reason,"outcome":"error"}),
            );
        }
        anyhow!(
            "Full-prefix episode summary unavailable ({reason}) for {model}; raw source remains uncovered. Configure summary_input=episode explicitly to use episode-only input"
        )
    }
}
/// Closes every episode-only summary request, after the transcript, so the
/// model does not take the transcript's last tool output as its turn to act.
pub(crate) const EPISODE_SUMMARY_CLOSE: &str = "END OF CURRENT EPISODE. Summarize the episode above. Do not continue it and do not call tools.";

/// Reject executable/protocol responses, without imposing a cosmetic length or
/// requiring one heading style. Prose may quote protocol evidence in a fenced
/// block or inline code span; unquoted markup anywhere is not memory.
pub(crate) fn summary_output_reason(kind: &str, text: &str) -> Option<&'static str> {
    let cleaned = if kind == "chapter" {
        text.lines()
            .filter(|line| !line.trim_start().starts_with("Related:"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_owned()
    };
    let text = cleaned.trim().trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Some("empty_output");
    }
    let body = if text.starts_with('#') {
        let (title, body) = text.split_once('\n').unwrap_or((text, ""));
        if title.trim_start_matches('#').trim().is_empty() {
            return Some("empty_title");
        }
        if body.trim().is_empty() {
            return Some("empty_summary_body");
        }
        body.trim()
    } else {
        text
    };
    let body = if body.starts_with("```") && body.ends_with("```") {
        body.split_once('\n')
            .map(|(_, value)| value[..value.len().saturating_sub(3)].trim())
            .unwrap_or(body)
    } else {
        body
    };
    let lower = body.to_lowercase();
    if ["[tool_calls]", "<|tool_call", "<|python_tag|>"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return Some("tool_protocol_output");
    }
    // A whole-body fence was unwrapped above, so its content is checked as the
    // output itself. Any other closed fenced block or inline code span is quoted
    // evidence; markup anywhere outside those quotes rejects the summary.
    if let Some(reason) = embedded_markup_reason(&unquoted_text(body)) {
        return Some(reason);
    }
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let is_call = |value: &Value| {
            value.get("tool_calls").is_some()
                || value.get("function_call").is_some()
                || ((value.get("name").is_some()
                    || value.get("tool").is_some()
                    || value.get("function").is_some())
                    && (value.get("arguments").is_some() || value.get("args").is_some()))
        };
        if is_call(&value)
            || value
                .as_array()
                .is_some_and(|items| !items.is_empty() && items.iter().all(is_call))
        {
            return Some("tool_action_output");
        }
    }
    static ACTION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    if ACTION.get_or_init(|| regex::Regex::new(r"^(?:(?:const|let|var)\s+[A-Za-z_$][A-Za-z0-9_$]*\s*=\s*)?(?:(?:return|await)\s+)*tools\.[A-Za-z_$][A-Za-z0-9_$]*\s*\(").unwrap()).is_match(body) {
        return Some("tool_action_output");
    }
    None
}

/// Tool-call protocol (DeepSeek DSML with any bar count or width, XML-style
/// call tags, file edit bodies) and the episode renderer's own transcript
/// labels. Either one means the model continued the transcript.
fn embedded_markup_reason(text: &str) -> Option<&'static str> {
    static PROTOCOL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static TRANSCRIPT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let protocol = PROTOCOL.get_or_init(|| {
        regex::Regex::new(
            r"(?i)<\s*/?\s*[|｜]*\s*dsml|</?\s*(?:invoke|tool_calls?|function_calls?)\b|<function=|<\|tool_call|<\|python_tag\|>|\*\*\*\s*begin patch",
        )
        .unwrap()
    });
    if protocol.is_match(text) {
        return Some("tool_protocol_output");
    }
    let transcript = TRANSCRIPT.get_or_init(|| {
        regex::Regex::new(
            r"(?m)^[ \t]*(?:Tool call \S+ \([^)\n]*\) arguments\b|Tool call ID: \S|\[tool output \S+, \d+ chars, (?:ok|error)\]|\[\S+ (?:User|Assistant|Tool|System)\][ \t]*$|\[source \S+: (?:Typed request|ChildTask instruction|Generated context))",
        )
        .unwrap()
    });
    transcript.is_match(text).then_some("transcript_output")
}

/// Text outside closed fenced blocks and inline code spans. An unclosed fence
/// is not a quote, so its content stays visible to validation.
fn unquoted_text(text: &str) -> String {
    let mut kept = Vec::new();
    let mut fence: Option<(&str, Vec<&str>)> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"]
            .into_iter()
            .find(|marker| trimmed.starts_with(marker));
        match (&mut fence, marker) {
            (Some((open, _)), Some(marker)) if *open == marker => fence = None,
            (Some((_, held)), _) => held.push(line),
            (None, Some(marker)) => fence = Some((marker, vec![line])),
            (None, None) => kept.push(line),
        }
    }
    if let Some((_, held)) = fence {
        kept.extend(held);
    }
    static INLINE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let inline = INLINE.get_or_init(|| regex::Regex::new(r"``[^\n]+?``|`[^`\n]+`").unwrap());
    inline.replace_all(&kept.join("\n"), "").into_owned()
}

/// Preserve decision-error consumers and the complete LLM billing record. The
/// decision usage type carries only input/output totals, not cache or reasoning.
fn with_episode_usage(error: anyhow::Error, usage: Usage) -> anyhow::Error {
    let error = crate::provider::with_decision_usage(error, usage.clone().into());
    let mut source = bubblepaw_core::ProviderError::of(&error)
        .cloned()
        .expect("with_decision_usage always supplies a classified provider error");
    source.usage = Some(usage);
    error.context(source)
}
#[cfg(test)]
#[path = "episode_models_tests.rs"]
mod tests;
fn trace_call(
    trace: Option<&JsonlTrace>,
    kind: &str,
    model: &str,
    usage: &Usage,
    started: Instant,
    outcome: &str,
    extra: Option<Value>,
) {
    if let Some(trace) = trace {
        let mut record = json!({"event":"episode_model_call","kind":kind,"model":model,"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cache_read_tokens":usage.cache_read_tokens,"cache_creation_tokens":usage.cache_creation_tokens,"reasoning_tokens":usage.reasoning_tokens,"latency_ms":started.elapsed().as_millis(),"outcome":outcome});
        // Extra fields (for example cache accounting metrics) merge into the record.
        if let (Some(Value::Object(fields)), Some(map)) = (extra, record.as_object_mut()) {
            map.extend(fields);
        }
        trace.runtime_info(record);
    }
}
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn tail(text: &str, count: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars[chars.len().saturating_sub(count)..].iter().collect()
}
fn summary_parts(text: &str, fallback: Option<&str>) -> (String, String) {
    if let Some((first, body)) = text.trim().split_once('\n')
        && let Some(title) = first.strip_prefix("# ").map(str::trim)
        && !title.is_empty()
        && !body.trim().is_empty()
    {
        return (title.into(), body.trim().into());
    }
    // A heading is presentation, not evidence. Derive bounded metadata without
    // rewriting accepted prose or fabricating a body.
    let title = text
        .lines()
        .chain(fallback.into_iter().flat_map(str::lines))
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .map(collapse)
        .unwrap_or_else(|| "Summary".into())
        .chars()
        .take(120)
        .collect();
    (title, text.into())
}

#[async_trait]
impl EpisodeModels for HttpEpisodeModels {
    fn gather_revision(&self) -> u64 {
        self.live.as_ref().map_or(0, |live| live.snapshot().id())
    }
    fn record_pressure_gather(&self, session: &str, force: bool) {
        if let Some(trace) = &self.trace {
            trace.runtime_info(json!({"event":"episode_pressure_gather", "session_id":session,
                "force":force,"source_policy":if force { "short_complete_range" } else { "ordinary" }}));
        }
    }
    fn record_fork_fallback(&self, session: &str, reason: &str) {
        if let Some(trace) = &self.trace {
            trace.runtime_info(json!({
                "event":"episode_fork_unavailable", "session_id":session,
                "reason":reason, "mode":"fork", "outcome":"error",
            }));
        }
    }
    async fn summarize_fork(
        &self,
        input: SummaryInput,
        fork: EpisodeFork,
    ) -> Result<SummaryResult> {
        let roles = self.current_models().0;
        let mut total = Usage::default();
        let mut last_error = None;
        let mut attempt = 0;
        let routes = fork.provider.summary_fork_routes(&fork.request.model);
        for (route_index, route) in routes.iter().enumerate() {
            let mut output_attempt = 1;
            loop {
                attempt += 1;
                match summarize_fork_attempt(self, &input, &fork, route, attempt).await {
                    Ok(mut result) => {
                        total.add(&result.usage);
                        result.usage = total;
                        return Ok(result);
                    }
                    Err(error) => {
                        if let Some(usage) = bubblepaw_core::ProviderError::of(&error)
                            .and_then(|error| error.usage.as_ref())
                        {
                            total.add(usage);
                        }
                        if bubblepaw_core::ProviderError::of(&error)
                            .is_some_and(|error| error.message == "cancelled")
                        {
                            return Err(with_episode_usage(error, total));
                        }
                        let invalid = error.downcast_ref::<InvalidMemoryOutput>().is_some();
                        let retry_output =
                            invalid && output_attempt < roles.summary.repeats.unwrap_or(1);
                        if let Some(trace) = &self.trace {
                            trace.runtime_info(json!({"event":"episode_fork_attempt_failed",
                                "mode":"fork","session_id":input.session_id,"model":route,"attempt":attempt,
                                "reason":if invalid { "invalid_memory_output" } else { "request_failed" },
                                "error":format!("{error:#}").chars().take(600).collect::<String>(),
                                "outcome":if retry_output { "retry_output" }
                                    else if routes.get(route_index+1).is_some() { "next_full_fork_route" } else { "exhausted" },
                                "next_route":routes.get(route_index+1),"input_policy_unchanged":true}));
                        }
                        last_error = Some(error);
                        if retry_output {
                            output_attempt += 1;
                            continue;
                        }
                        // Transport retries belong to the registry's policy.
                        break;
                    }
                }
            }
        }
        Err(with_episode_usage(
            last_error.unwrap_or_else(|| {
                anyhow!("No compatible full-prefix episode route is configured")
            }),
            total,
        ))
    }
    fn record_shutdown_abort(
        &self,
        aborted_calls: usize,
        completed_results_uncommitted: usize,
        drain_timeout: std::time::Duration,
    ) {
        if let Some(trace) = &self.trace {
            trace.runtime_info(json!({
                "event":"episode_drain_timeout", "aborted_calls":aborted_calls,
                "completed_results_uncommitted":completed_results_uncommitted,
                "drain_timeout_seconds":drain_timeout.as_secs_f64(),
                "usage_note":"Unfinished physical calls have unknown usage. Completed attempt usage is retained in the chat ledger even when the derived result is not committed.",
                "outcome":"error",
            }));
        }
    }
    fn record_drain_failure(&self, session: &str, error: &anyhow::Error) {
        let Some(trace) = &self.trace else { return };
        let provider = bubblepaw_core::ProviderError::of(error);
        let kind = match provider.map(|error| &error.kind) {
            Some(bubblepaw_core::ProviderErrorKind::Auth) => "auth",
            Some(bubblepaw_core::ProviderErrorKind::RateLimit { .. }) => "rate_limit",
            Some(bubblepaw_core::ProviderErrorKind::Context) => "context",
            Some(bubblepaw_core::ProviderErrorKind::Transient) => "transient",
            Some(bubblepaw_core::ProviderErrorKind::Fatal) => "fatal",
            None => "internal",
        };
        let usage = provider
            .and_then(|error| error.usage.clone())
            .or_else(|| decision_error_usage(error).cloned().map(Usage::from));
        // Error strings can contain provider payloads. Keep only categorical
        // metadata; trace queue/write failures remain visible via TraceStatus.
        trace.runtime_info(json!({
            "event":"episode_drain_failed", "session_id":session,
            "error_kind":kind, "usage":usage, "usage_scope":"failed_operation",
        }));
    }
    fn record_fork_context(&self, session: &str, request: &ModelRequest, cold: &[usize]) {
        self.context_route.record_fork(session, request, cold);
    }
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let (roles, registry) = self.current_models();
        let ids: Vec<String> = (1..=input.options.len())
            .map(|id| id.to_string())
            .chain(std::iter::once("none".into()))
            .collect();
        if input.options.is_empty() {
            let candidate = roles
                .decider
                .models
                .iter()
                .find(|model| registry.decision_available(model));
            let model = candidate
                .map(|model| registry.decision_model_name(model))
                .unwrap_or_default();
            return Ok(SplitResult {
                choice: None,
                probabilities: vec![1.0],
                model,
                usage: Usage::default(),
                reasoning: None,
            });
        }
        roles.decider.require_configured("decider")?;
        let lines = input
            .options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                format!(
                    "{}. (agent before: {}) {}",
                    i + 1,
                    collapse(&tail(&option.agent_before, 400)),
                    crate::episodes::token_excerpt(
                        &collapse(&option.user),
                        self.episode_config.summary_target_tokens
                    )
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let state = format!(
            "ACCUMULATED OPEN CHAPTER STATE:\n{}\n\nNEXT BOUNDARIES (typed request or labeled episode summary):\n{}",
            crate::episodes::token_excerpt(
                &input.open_chapter,
                self.episode_config.decision_context_tokens
            ),
            lines
        );
        let request = DecisionRequest {
            reasoning_effort: roles.decider.reasoning_effort,
            model: String::new(),
            input: state,
            questions: vec![Question {
                name: "choice".into(),
                instructions: SPLIT_QUESTION.into(),
                kind: QuestionKind::Choice {
                    choices: ids
                        .iter()
                        .map(|id| DecisionChoice {
                            value: id.clone(),
                            description: if id == "none" {
                                "No listed option applies.".into()
                            } else {
                                format!("Select option {id} from the state.")
                            },
                        })
                        .collect(),
                },
            }],
            session_id: Some(input.session_id.clone()),
        };
        let trace_role = roles.decider.clone();
        let (trace, trace_route) = (self.trace.clone(), self.context_route.clone());
        let trace_registry = registry.clone();
        let trace_request = request.clone();
        let session_id = input.session_id.clone();
        let (response, reasoning, selected) = self.context_route.scope(roles.decider
            .decide_with_retry_selected(
                &registry,
                request,
                CancellationToken::new(),
                self.config.retry_base_delay,
                Arc::new(move |candidate, result, started| {
                    let (repeats, threshold) = trace_role.decision_settings(candidate);
                    let usage: Usage = result
                        .as_ref()
                        .ok()
                        .map(|r| r.usage.clone().into())
                        .or_else(|| {
                            result
                                .as_ref()
                                .err()
                                .and_then(decision_error_usage)
                                .cloned()
                                .map(Into::into)
                        })
                        .unwrap_or_default();
                    let chat = trace_registry.decision_uses_llm(candidate);
                    let outcome = if result.is_err()
                        && result
                            .as_ref()
                            .err()
                            .and_then(decision_error_usage)
                            .is_none()
                    {
                        "error"
                    } else if chat
                        && result.as_ref().ok().is_some_and(|response| {
                            response
                                .answers
                                .iter()
                                .any(|answer| matches!(answer.kind, AnswerKind::Refusal))
                        })
                    {
                        "invalid_chat_none"
                    } else {
                        "ok"
                    };
                    let model = trace_registry.decision_model_name(candidate);
                    if let Some(trace) = &trace {
                        trace.runtime_info(json!({"event":"episode_decision_calibration","model":candidate,"repeats":repeats,"threshold":threshold}));
                    }
                    // Measure chat decisions against the shared prompt cache. Their
                    // request is built below this observer, so it is rebuilt here;
                    // priors only change on finish, so starting late is equivalent.
                    // RequestKind::Decider never declares or causes a main-loop break.
                    let cache_metrics = trace.as_ref().and_then(|_| {
                        let shape = trace_registry.decision_wire_shape(candidate, &trace_request)?;
                        let observed = result.as_ref().ok().map(|response| &response.usage).or_else(|| {
                            result.as_ref().err().and_then(decision_error_usage)
                        })?;
                        // A fallback route reports its own model; compare against the plan.
                        let selected = result
                            .as_ref()
                            .ok()
                            .filter(|response| response.model != shape.model)
                            .map(|response| format!("{}/{}", shape.provider, response.model));
                        let id = uuid::Uuid::new_v4().to_string();
                        let cache = trace_route.cache();
                        cache.start(
                            &id,
                            &session_id,
                            RequestKind::Decider,
                            shape,
                            Vec::new(),
                        );
                        cache
                            .finish(&id, Some(&observed.clone().into()), selected.as_deref(), true)
                            .map(|outcome| outcome.metrics())
                    });
                    trace_call(
                        trace.as_ref(),
                        if chat { "decider_chat" } else { "decider" },
                        &model,
                        &usage,
                        started,
                        outcome,
                        cache_metrics,
                    );
                }),
            ))
            .await?;
        let (_, threshold) = roles.decider.decision_settings(&selected);
        let answer = response
            .answers
            .iter()
            .find(|answer| answer.name == "choice")
            .ok_or_else(|| anyhow!("Decision response has no split answer"))?;
        let mut probabilities = vec![0.0; ids.len()];
        match &answer.kind {
            AnswerKind::Choice {
                probabilities: distribution,
                ..
            } => {
                ensure!(
                    distribution.len() == ids.len(),
                    "Decision probabilities do not match options"
                );
                for (i, id) in ids.iter().enumerate() {
                    probabilities[i] = distribution
                        .iter()
                        .find(|(value, _)| value == id)
                        .map(|(_, p)| *p)
                        .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
                        .ok_or_else(|| anyhow!("Invalid decision probability"))?;
                }
                let sum: f64 = probabilities.iter().sum();
                ensure!(
                    (sum - 1.0).abs() <= 1e-3,
                    "Decision probabilities do not sum to one"
                );
                for p in &mut probabilities {
                    *p /= sum;
                }
            }
            AnswerKind::Refusal => probabilities[ids.len() - 1] = 1.0,
            _ => return Err(anyhow!("Decision response is not a choice")),
        }
        let none_probability = probabilities[ids.len() - 1];
        let choice = probabilities[..ids.len() - 1]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(&a.0)))
            .filter(|(_, probability)| {
                if threshold == 0.0 {
                    // No confidence gate still permits the model to choose none.
                    // Prefer none on a tie rather than inventing a transition.
                    **probability > none_probability
                } else {
                    1.0 - none_probability + 1e-12 >= threshold
                }
            })
            .map(|(i, _)| input.options[i].message_id.clone());
        Ok(SplitResult {
            choice,
            probabilities,
            model: response.model,
            usage: response.usage.into(),
            reasoning,
        })
    }
    async fn summarize(&self, input: SummaryInput) -> Result<SummaryResult> {
        if let Some(trace) = &self.trace {
            trace.runtime_info(json!({"event":"episode_summary_input", "mode":"episode",
                "policy":"explicit", "session_id":input.session_id}));
        }
        let system = fork_requests::summary_instructions(self, &input);
        let turn = input
            .turn_request
            .as_ref()
            .map(|request| format!("TASK ANCHOR (read-only, may be a child task):\n{request}\n\n"))
            .unwrap_or_default();
        let user = format!(
            "{turn}PREVIOUS EPISODE (read-only):\n{}\n\nCURRENT EPISODE:\n{}\n\n{EPISODE_SUMMARY_CLOSE}",
            input.previous_summary.as_deref().unwrap_or("none"),
            input.episode_text
        );
        let response = self
            .chat(
                "summary",
                &input.session_id,
                system,
                user,
                Some(bubblepaw_core::ContextState::SIDE_CALL_EPISODE_INPUT),
            )
            .await?;
        self.validate_summary_output("summary", &input.session_id, &response, &response.text)?;
        let (title, _) = summary_parts(&response.text, input.turn_request.as_deref());
        Ok(SummaryResult {
            title,
            summary: response.text,
            model: response.model,
            usage: response.usage,
            reasoning: response.reasoning,
        })
    }
    async fn summarize_classical(&self, input: ClassicalInput) -> Result<ClassicalResult> {
        let user = format!(
            "PREVIOUS SUMMARY (read-only):\n{}\n\nLATEST TYPED USER REQUEST (verbatim, read-only):\n{}\n\nNEW HISTORY SPAN (read-only):\n{}",
            input.previous_summary.as_deref().unwrap_or("none"),
            input.latest_user_request.as_deref().unwrap_or("none"),
            input.span_text
        );
        let response = self
            .chat(
                "classical",
                &input.session_id,
                CLASSICAL_PROMPT.into(),
                user,
                None,
            )
            .await?;
        // Length guidance belongs to the prompt. Keep the output intact; a length
        // stop is continued by the provider, so no size check happens here.
        Ok(ClassicalResult {
            summary: response.text,
            model: response.model,
            usage: response.usage,
            reasoning: response.reasoning,
        })
    }
    async fn close_chapter(&self, input: ChapterInput) -> Result<ChapterResult> {
        chapter_requests::close_chapter(self, input, None).await
    }
    async fn close_chapter_fork(
        &self,
        input: ChapterInput,
        fork: EpisodeFork,
    ) -> Result<ChapterResult> {
        chapter_requests::close_chapter(self, input, Some(fork)).await
    }
}
/// Invalid tags never discard the chapter; only offered IDs receive links.
pub fn parse_related(
    text: &str,
    previous: &[ContextSubjectChapter],
    users: &[String],
) -> (String, Vec<String>, Vec<RelatedEvidence>) {
    parse_related_with_tasks(text, previous, users, &[])
}
pub fn parse_related_with_tasks(
    text: &str,
    previous: &[ContextSubjectChapter],
    users: &[String],
    tasks: &[String],
) -> (String, Vec<String>, Vec<RelatedEvidence>) {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let clean = lines
        .iter()
        .filter(|line| !line.trim_start().starts_with("Related:"))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if lines
        .iter()
        .filter(|line| line.trim_start().starts_with("Related:"))
        .count()
        != 1
    {
        return (clean, Vec::new(), Vec::new());
    }
    let Some(value) = lines.last().and_then(|line| line.strip_prefix("Related: ")) else {
        return (clean, Vec::new(), Vec::new());
    };
    if value == "none" {
        return (clean, Vec::new(), Vec::new());
    }
    let expression = regex::Regex::new(
        r#"(?:^|;)\s*([^\s();]+)\s*\((user|request|task):\s*\"((?:\\.|[^\"\\])*)\"\s*\)\s*(?:$|;)"#,
    )
    .unwrap();
    let mut ids = Vec::new();
    let mut evidence = Vec::new();
    // Do not split on semicolons within quoted text.
    let entries = split_related_entries(value);
    for entry in entries {
        let Some(captures) = expression.captures(entry) else {
            continue;
        };
        let id = &captures[1];
        if !previous
            .iter()
            .filter(|chapter| chapter.status == bubblepaw_core::SubjectChapterStatus::Closed)
            .rev()
            .take(8)
            .any(|chapter| chapter.id == id)
            || ids.iter().any(|existing| existing == id)
        {
            continue;
        }
        let quote = captures[3].replace("\\\"", "\"").replace("\\\\", "\\");
        let needle = collapse(&quote).to_lowercase();
        let matches = |sources: &[String]| {
            !needle.is_empty()
                && sources.iter().any(|source| {
                    !source.trim_start().starts_with("<turn_aborted>")
                        && collapse(source).to_lowercase().contains(&needle)
                })
        };
        // Provenance is established from the actual input, not the model's label.
        // Neutral request labels prefer a typed source when the same text exists in both.
        let origin = match &captures[2] {
            "user" if matches(users) => Some(MessageOrigin::Typed),
            "task" if matches(tasks) => Some(MessageOrigin::ChildTask),
            "request" if matches(users) => Some(MessageOrigin::Typed),
            "request" if matches(tasks) => Some(MessageOrigin::ChildTask),
            _ => None,
        };
        ids.push(id.into());
        evidence.push(RelatedEvidence {
            quote,
            related_quote_matched: Some(origin.is_some()),
            origin,
        });
    }
    (clean, ids, evidence)
}
fn split_related_entries(value: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut depth = 0_usize;
    for (i, c) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        match c {
            '(' => depth += 1,
            ')' => {
                let Some(next) = depth.checked_sub(1) else {
                    return Vec::new();
                };
                depth = next;
            }
            ';' if depth == 0 => {
                entries.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if quoted || depth != 0 {
        return Vec::new();
    }
    entries.push(&value[start..]);
    entries
}
