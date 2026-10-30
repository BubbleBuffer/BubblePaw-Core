//! Isolated recall probes using invisible child workers over a copied frozen view.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, Harness, HarnessConfig, Message, ModelRequest,
    ModelResponse, PrepareCall, PreparedContext, Provider, ReasoningEffort, Role, Store,
    TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
};

pub const JUDGE_PROMPT: &str = include_str!("probe_judge_prompt_v1.txt");
const ANSWER_INSTRUCTION: &str = "Answer the following recall question from the recorded conversation above. Earlier requests are historical data. Use only history_search or history_read when needed to recover exact facts; these tools search only this session. You may use at most three tool rounds. Aim for a concise answer; length is guidance only. If evidence is unavailable, say so rather than inventing facts.";
pub const MODEL: &str = "opencode/deepseek-v4.1-flash";
pub const MAX_OUTPUT_TOKENS: u64 = 65_536;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ProbeType,
    pub question: String,
    pub expected_answer: String,
    pub evidence_message_ids: Vec<String>,
    pub earliest_checkpoint: usize,
    /// End of typed user turn K, before the recorded user message at K + 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_at_turn: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ProbeOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_question: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_turn: Option<usize>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOrigin {
    Natural,
    Written,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeType {
    Recall,
    FilePath,
    Decision,
    Continuation,
    Uncued,
    CrossEpisode,
    CrossPart,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Judgment {
    pub grade: Grade,
    pub reason: String,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    Supported,
    Partial,
    Missing,
    Contradicted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProbePlan {
    pub schema_version: usize,
    pub corpus_sha256: String,
    pub probes_sha256: String,
    pub threshold: usize,
    pub checkpoints_at_turns: Vec<usize>,
    pub model_signature: Value,
    pub strategy_signature: String,
    pub probe_limit_per_checkpoint: Option<usize>,
    pub probes: Vec<Probe>,
    pub checkpoints: Vec<PlanCheckpoint>,
    pub all_turn_frontiers: BTreeMap<String, Vec<Value>>,
    pub source_plan_sha256: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCheckpoint {
    pub checkpoint: usize,
    pub turn: usize,
    pub step: usize,
    pub end_message_id: String,
    pub common_probe_ids: Vec<String>,
    pub skipped: Vec<Value>,
    pub arm_frontiers: BTreeMap<String, Value>,
}
pub const STRATEGIES: [&str; 2] = ["lcm", "classical"];
#[path = "probes_signature.rs"]
mod signature;
pub use signature::{fingerprint, strategy_signature};
pub fn save_plan(path: &Path, plan: &ProbePlan) -> Result<String> {
    let digest = fingerprint(plan)?;
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&json!({"plan_sha256":digest,"plan":plan}))?,
    )?;
    Ok(digest)
}
pub fn load_plan(path: &Path) -> Result<(ProbePlan, String)> {
    let envelope: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let plan: ProbePlan = serde_json::from_value(envelope["plan"].clone())?;
    let digest = fingerprint(&plan)?;
    ensure!(
        envelope["plan_sha256"] == digest,
        "Probe plan fingerprint mismatch"
    );
    ensure!(plan.schema_version == 1, "Unsupported probe plan schema");
    ensure!(
        plan.probes_sha256 == fingerprint(&plan.probes)?,
        "Pinned probe set fingerprint mismatch"
    );
    ensure!(
        plan.strategy_signature == strategy_signature(),
        "Probe plan strategy implementation/prompt identity changed"
    );
    ensure!(
        plan.all_turn_frontiers
            .keys()
            .map(String::as_str)
            .collect::<HashSet<_>>()
            == STRATEGIES.into_iter().collect(),
        "Probe plan must pin exactly lcm and classical"
    );
    ensure!(
        plan.checkpoints_at_turns.len() == plan.checkpoints.len(),
        "Probe plan checkpoint count mismatch"
    );
    ensure!(
        !plan.checkpoints_at_turns.is_empty()
            && plan
                .checkpoints_at_turns
                .windows(2)
                .all(|pair| pair[0] < pair[1])
            && plan.checkpoints_at_turns[0] > 0,
        "Probe plan turn positions must be positive and increasing"
    );
    for (index, checkpoint) in plan.checkpoints.iter().enumerate() {
        ensure!(
            checkpoint.checkpoint == index + 1
                && checkpoint.turn == plan.checkpoints_at_turns[index],
            "Probe plan checkpoint ordinal/position mismatch"
        );
        ensure!(
            checkpoint
                .arm_frontiers
                .keys()
                .map(String::as_str)
                .collect::<HashSet<_>>()
                == STRATEGIES.into_iter().collect(),
            "Probe checkpoint lacks an arm frontier"
        );
        let selected: HashSet<_> = checkpoint.common_probe_ids.iter().collect();
        ensure!(
            selected.len() == checkpoint.common_probe_ids.len()
                && checkpoint
                    .common_probe_ids
                    .iter()
                    .all(|id| plan.probes.iter().any(|p| &p.id == id)),
            "Probe plan contains duplicate or unknown common IDs"
        );
    }
    Ok((plan, digest))
}
pub fn common_checkpoints(
    probes: &[Probe],
    history: &[Message],
    turns: &[usize],
    all_turn_frontiers: &BTreeMap<String, Vec<Value>>,
    limit: Option<usize>,
) -> Result<Vec<PlanCheckpoint>> {
    let mut checkpoints = Vec::new();
    for (index, turn) in turns.iter().enumerate() {
        let ordinal = index + 1;
        let mut arm_frontiers = BTreeMap::new();
        for strategy in STRATEGIES {
            let frontier = all_turn_frontiers
                .get(strategy)
                .and_then(|rows| {
                    rows.iter()
                        .find(|row| row["turn"].as_u64() == Some(*turn as u64))
                })
                .context(format!("Missing {strategy} end-of-turn {turn} frontier"))?;
            ensure!(
                frontier["status"] == "complete",
                "Incomplete recorded tool round at turn {turn}"
            );
            arm_frontiers.insert(strategy.to_owned(), frontier.clone());
        }
        let reference = arm_frontiers.values().next().unwrap();
        ensure!(
            arm_frontiers
                .values()
                .all(|row| row["step"] == reference["step"]
                    && row["end_message_id"] == reference["end_message_id"]),
            "Strategy checkpoint positions differ"
        );
        let mut common_probe_ids = Vec::new();
        let mut skipped = Vec::new();
        for probe in probes {
            if !at_position(probe, *turn) {
                skipped.push(json!({"id":probe.id,"type":probe.kind,"skip_reason":"different_ask_turn","ask_at_turn":probe.ask_at_turn,"uncovered_arms":[]}));
                continue;
            }
            let uncovered: Vec<_> = STRATEGIES
                .into_iter()
                .filter(|strategy| {
                    !eligible(
                        probe,
                        ordinal,
                        *turn,
                        history,
                        arm_frontiers[*strategy]["frontier_message_id"].as_str(),
                    )
                })
                .collect();
            let reason = if ordinal < probe.earliest_checkpoint {
                Some("before_earliest_checkpoint")
            } else if !uncovered.is_empty() {
                Some("evidence_not_compacted_in_all_arms")
            } else if limit.is_some_and(|limit| common_probe_ids.len() >= limit) {
                Some("probe_limit")
            } else {
                None
            };
            if let Some(reason) = reason {
                skipped.push(json!({"id":probe.id,"type":probe.kind,"skip_reason":reason,"uncovered_arms":uncovered}));
            } else {
                common_probe_ids.push(probe.id.clone());
            }
        }
        checkpoints.push(PlanCheckpoint {
            checkpoint: ordinal,
            turn: *turn,
            step: reference["step"]
                .as_u64()
                .context("Checkpoint lacks step")? as usize,
            end_message_id: reference["end_message_id"]
                .as_str()
                .context("Checkpoint lacks end message")?
                .into(),
            common_probe_ids,
            skipped,
            arm_frontiers,
        });
    }
    Ok(checkpoints)
}

pub fn load(path: &Path, history: &[Message]) -> Result<Vec<Probe>> {
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let probes: Vec<Probe> = serde_json::from_value(value.get("probes").unwrap_or(&value).clone())?;
    validate(&probes, history)?;
    Ok(probes)
}

pub fn validate(probes: &[Probe], history: &[Message]) -> Result<()> {
    ensure!(!probes.is_empty(), "Probe set is empty");
    let history_ids: HashSet<_> = history.iter().map(|m| m.id.as_str()).collect();
    let typed_turns: Vec<_> = history
        .iter()
        .filter(|m| bubblepaw_core::is_typed_user(m))
        .collect();
    let mut ids = HashSet::new();
    for probe in probes {
        ensure!(
            !probe.id.trim().is_empty() && ids.insert(&probe.id),
            "Empty or duplicate probe ID"
        );
        ensure!(
            !probe.question.trim().is_empty() && !probe.expected_answer.trim().is_empty(),
            "Probe {} lacks question or expected answer",
            probe.id
        );
        ensure!(
            probe.earliest_checkpoint > 0 && !probe.evidence_message_ids.is_empty(),
            "Probe {} lacks checkpoint or evidence",
            probe.id
        );
        ensure!(
            probe
                .ask_at_turn
                .is_none_or(|turn| turn > 0 && turn <= typed_turns.len()),
            "Probe {} ask_at_turn is outside the typed-user turn range",
            probe.id
        );
        ensure!(
            probe
                .original_question
                .as_ref()
                .is_none_or(|question| !question.trim().is_empty()),
            "Probe {} has an empty original question",
            probe.id
        );
        if let Some(turn) = probe.source_turn {
            ensure!(
                turn > 0 && turn <= typed_turns.len(),
                "Probe {} source_turn is outside the typed-user turn range",
                probe.id
            );
            if let Some(id) = &probe.source_message_id {
                ensure!(
                    typed_turns[turn - 1].id == *id,
                    "Probe {} source message and typed turn differ",
                    probe.id
                );
            }
        }
        if let Some(id) = &probe.source_message_id {
            ensure!(
                history_ids.contains(id.as_str()),
                "Probe {} cites missing source message {}",
                probe.id,
                id
            );
        }
        if probe.origin == Some(ProbeOrigin::Natural) {
            ensure!(
                probe.original_question.is_some()
                    && probe.source_message_id.is_some()
                    && probe.source_turn.is_some(),
                "Natural probe {} lacks original-question provenance",
                probe.id
            );
            ensure!(
                probe.source_turn.and_then(|turn| turn.checked_sub(1)) == probe.ask_at_turn,
                "Natural probe {} must be asked just before its source user turn",
                probe.id
            );
        }
        let mut evidence = HashSet::new();
        for id in &probe.evidence_message_ids {
            ensure!(
                history_ids.contains(id.as_str()),
                "Probe {} cites missing message {}",
                probe.id,
                id
            );
            if let Some(turn) = probe.ask_at_turn {
                let end = typed_turns
                    .get(turn)
                    .and_then(|next| history.iter().position(|m| m.id == next.id))
                    .unwrap_or(history.len());
                ensure!(
                    history[..end].iter().any(|m| &m.id == id),
                    "Probe {} evidence {} occurs after its ask position",
                    probe.id,
                    id
                );
            }
            ensure!(
                evidence.insert(id),
                "Probe {} cites duplicate evidence {}",
                probe.id,
                id
            );
        }
    }
    Ok(())
}
pub fn at_position(probe: &Probe, turn: usize) -> bool {
    probe.ask_at_turn.is_none_or(|position| position == turn)
}

/// Positioned sets default to one shared checkpoint per distinct ask position.
/// Empty means that replay should use its existing legacy checkpoint defaults.
pub fn ask_positions(probes: &[Probe]) -> Vec<usize> {
    let mut positions: Vec<_> = probes
        .iter()
        .filter_map(|probe| probe.ask_at_turn)
        .collect();
    positions.sort_unstable();
    positions.dedup();
    positions
}

pub fn eligible(
    probe: &Probe,
    checkpoint: usize,
    turn: usize,
    history: &[Message],
    frontier_id: Option<&str>,
) -> bool {
    let Some(frontier) = frontier_id.and_then(|id| history.iter().position(|m| m.id == id)) else {
        return false;
    };
    at_position(probe, turn)
        && checkpoint >= probe.earliest_checkpoint
        && probe.evidence_message_ids.iter().all(|id| {
            history
                .iter()
                .position(|m| &m.id == id)
                .is_some_and(|i| i <= frontier)
        })
}
pub fn parse_judgment(text: &str) -> Result<Judgment> {
    let judgment: Judgment = serde_json::from_str(text).context("Judge JSON/schema failure")?;
    ensure!(
        !judgment.reason.trim().is_empty() && !judgment.reason.contains(['\n', '\r']),
        "Judge reason must be one nonempty line"
    );
    Ok(judgment)
}

/// One physical request, without hidden retries. Replay supplies the common spend relay.
#[async_trait]
pub trait ProbeModel: Send + Sync {
    fn model(&self) -> &str {
        MODEL
    }
    fn reasoning_effort(&self) -> ReasoningEffort {
        ReasoningEffort::Low
    }
    async fn call(&self, body: Value, kind: &str) -> Result<Value>;
    /// Diagnostic lineage is separate from transport session/cursor metadata.
    async fn call_in_session(&self, body: Value, kind: &str, _session: &str) -> Result<Value> {
        self.call(body, kind).await
    }
    async fn close_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }
}

fn wire(message: &Message) -> Value {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut item = json!({"role":role,"content":message.content,"id":message.id,"session_id":message.session_id,"origin":message.origin,"is_error":message.is_error});
    if !message.tool_calls.is_empty() {
        item["tool_calls"] = json!(message.tool_calls.iter().map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}})).collect::<Vec<_>>());
    }
    if let Some(id) = &message.tool_call_id {
        item["tool_call_id"] = json!(id);
    }
    if let Some(state) = &message.provider_state {
        item["provider_state"] = state.clone();
    }
    item
}
/// Convert the probe's transient Chat-shaped exchange into the shared provider API.
/// Provider state stays on assistant messages so Messages thinking blocks survive tools.
pub(crate) fn provider_request(
    body: &Value,
    model: &str,
    effort: ReasoningEffort,
) -> Result<ModelRequest> {
    let mut request = ModelRequest {
        model: model.into(),
        session_id: body["_bubblepaw_request"]["session_id"]
            .as_str()
            .unwrap_or_default()
            .into(),
        history: body["_bubblepaw_request"]
            .get("history")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default(),
        reasoning_effort: Some(effort),
        max_output_tokens: Some(u32::try_from(
            body["max_tokens"].as_u64().unwrap_or(MAX_OUTPUT_TOKENS),
        )?),
        ..Default::default()
    };
    for item in body["messages"]
        .as_array()
        .context("Probe lacks messages")?
    {
        let role = match item["role"].as_str() {
            Some("system") => Role::System,
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            Some("tool") => Role::Tool,
            _ => anyhow::bail!("Unknown probe message role"),
        };
        let mut calls = Vec::new();
        for call in item["tool_calls"].as_array().into_iter().flatten() {
            calls.push(ToolCall {
                id: call["id"].as_str().context("Probe tool lacks ID")?.into(),
                name: call["function"]["name"]
                    .as_str()
                    .context("Probe tool lacks name")?
                    .into(),
                arguments: serde_json::from_str(
                    call["function"]["arguments"]
                        .as_str()
                        .context("Probe tool lacks arguments")?,
                )?,
            });
        }
        request.messages.push(Message {
            id: item["id"].as_str().unwrap_or_default().into(),
            session_id: item["session_id"].as_str().unwrap_or_default().into(),
            role,
            origin: item
                .get("origin")
                .cloned()
                .filter(|v| !v.is_null())
                .map(serde_json::from_value)
                .transpose()?,
            content: item["content"].as_str().unwrap_or_default().into(),
            tool_calls: calls,
            tool_call_id: item["tool_call_id"].as_str().map(str::to_owned),
            provider_state: item
                .get("provider_state")
                .cloned()
                .filter(|state| !state.is_null()),
            is_error: item["is_error"].as_bool().unwrap_or(false),
            created_at: String::new(),
        });
    }
    for item in body["tools"].as_array().into_iter().flatten() {
        let tool = &item["function"];
        request.tools.push(ToolDefinition {
            name: tool["name"]
                .as_str()
                .context("Probe tool lacks name")?
                .into(),
            description: tool["description"].as_str().unwrap_or_default().into(),
            input_schema: tool["parameters"].clone(),
            read_only: true,
            approval: Default::default(),
        });
    }
    Ok(request)
}
pub(crate) fn provider_payload(response: ModelResponse) -> Value {
    let usage = response.usage.unwrap_or_default();
    let mut message = json!({"role":"assistant","content":response.text,"provider_state":response.provider_state});
    if !response.tool_calls.is_empty() {
        message["tool_calls"] = json!(response.tool_calls.iter().map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}})).collect::<Vec<_>>());
    }
    json!({"choices":[{"message":message,"finish_reason":if response.tool_calls.is_empty() {"stop"} else {"tool_calls"}}],"usage":{"prompt_tokens":usage.input_tokens,"completion_tokens":usage.output_tokens,"cache_read_tokens":usage.cache_read_tokens,"cache_creation_tokens":usage.cache_creation_tokens,"completion_tokens_details":{"reasoning_tokens":usage.reasoning_tokens}}})
}
fn request(messages: &[Value], tools: &[Value]) -> Value {
    let mut body = json!({"model":"deepseek-v4.1-flash","messages":messages,"max_tokens":MAX_OUTPUT_TOKENS,"stream":false,"thinking":{"type":"enabled"},"reasoning_effort":"low"});
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
    }
    body
}
fn payload_message(payload: &Value) -> Result<&Value> {
    payload
        .pointer("/choices/0/message")
        .filter(|m| m.is_object())
        .context("Model response has no message")
}
fn usage(payload: &Value) -> Value {
    let input = payload["usage"]["prompt_tokens"]
        .as_u64()
        .or_else(|| payload["usage"]["input_tokens"].as_u64());
    let output = payload["usage"]["completion_tokens"]
        .as_u64()
        .or_else(|| payload["usage"]["output_tokens"].as_u64());
    json!({"input_tokens":input,"output_tokens":output,"cache_read_tokens":payload["usage"]["cache_read_tokens"],"cache_creation_tokens":payload["usage"]["cache_creation_tokens"],"reasoning_tokens":payload["usage"]["completion_tokens_details"]["reasoning_tokens"]})
}

/// A delivered probe result. Acknowledge only after its containing artifact is saved.
pub struct ProbeAnswer {
    value: Value,
    worker: Option<(crate::agents::AgentTools, String, String, String)>,
}
impl std::ops::Deref for ProbeAnswer {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.value
    }
}
impl ProbeAnswer {
    pub fn worker_session_id(&self) -> Option<&str> {
        self.worker
            .as_ref()
            .map(|(_, _, session, _)| session.as_str())
    }
    pub fn worker_run_id(&self) -> Option<&str> {
        self.worker.as_ref().map(|(_, _, _, run)| run.as_str())
    }
    pub async fn acknowledge(self) -> Result<()> {
        if self.value["status"] == "answered"
            && let Some((agents, owner, address, run_id)) = self.worker
        {
            agents
                .acknowledge_worker_turn(&owner, &address, &run_id)
                .await?;
        }
        Ok(())
    }
}

/// Dropping a host request still signals the ordinary child observer to clean up.
struct ProbeCancelOnDrop(CancellationToken);
impl Drop for ProbeCancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct FrozenProbeContext {
    frozen: ModelRequest,
    effort: ReasoningEffort,
}
#[async_trait]
impl ContextPreparation for FrozenProbeContext {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let mut request = call.request;
        let tool_rounds = request
            .messages
            .iter()
            .filter(|m| !m.tool_calls.is_empty())
            .count();
        if tool_rounds >= 3 {
            request.tools.clear();
        }
        let mut messages = self.frozen.messages.clone();
        messages.extend(request.messages);
        request.messages = messages;
        request.system_prompt = format!("{}\n\n{}", self.frozen.system_prompt, ANSWER_INSTRUCTION);
        request.reasoning_effort = Some(self.effort);
        request.max_output_tokens = Some(u32::try_from(MAX_OUTPUT_TOKENS)?);
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
}

/// History evidence belongs to the source chat, never to the worker conversation.
struct ProbeHistoryTool {
    source: String,
    tool: Arc<dyn Tool>,
}
#[async_trait]
impl Tool for ProbeHistoryTool {
    fn definition(&self) -> ToolDefinition {
        self.tool.definition()
    }
    async fn invoke(&self, mut context: ToolContext, input: Value) -> Result<ToolResult> {
        context.session_id = self.source.clone();
        self.tool.invoke(context, input).await
    }
}

/// Only translates the probe transport; the canonical harness owns every tool round.
struct ProbeProvider {
    model: Arc<dyn ProbeModel>,
    calls: Arc<Mutex<Vec<Value>>>,
}
#[async_trait]
impl Provider for ProbeProvider {
    async fn generate(
        &self,
        request_view: ModelRequest,
        _on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let mut messages = vec![json!({"role":"system","content":request_view.system_prompt})];
        messages.extend(request_view.messages.iter().map(wire));
        let tools: Vec<_> = request_view.tools.iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}})).collect();
        let mut body = request(&messages, &tools);
        // Host-only identity/cursor metadata, removed before raw provider transport.
        // Keep one copy of the actual message/tool exchange for spend calibration.
        body["_bubblepaw_request"] =
            json!({"session_id":request_view.session_id,"history":request_view.history});
        body["reasoning_effort"] = json!(
            request_view
                .reasoning_effort
                .unwrap_or(ReasoningEffort::Low)
                .as_str()
        );
        let payload = tokio::select! {
            _ = cancellation.cancelled() => anyhow::bail!("Probe cancelled"),
            result = self.model.call(body, "probe") => result?,
        };
        let mut diagnostics = self.calls.lock().unwrap();
        let round = diagnostics.len();
        diagnostics.push(json!({"round":round,"usage":usage(&payload),"finish_reason":payload["choices"][0]["finish_reason"],"requested_tool_calls":payload["choices"][0]["message"]["tool_calls"]}));
        drop(diagnostics);
        let item = payload_message(&payload)?;
        let mut calls = Vec::new();
        for call in item["tool_calls"].as_array().into_iter().flatten() {
            let name = call["function"]["name"]
                .as_str()
                .context("Tool call lacks name")?;
            ensure!(
                request_view.tools.iter().any(|t| t.name == name),
                "Probe requested unavailable tool {name}"
            );
            calls.push(ToolCall {
                id: call["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .context("Tool call lacks ID")?
                    .into(),
                name: name.into(),
                arguments: serde_json::from_str(
                    call["function"]["arguments"]
                        .as_str()
                        .context("Tool arguments are not JSON text")?,
                )?,
            });
        }
        let text = item["content"].as_str().unwrap_or_default().to_owned();
        ensure!(
            !calls.is_empty() || !text.is_empty(),
            "Probe answer has no text"
        );
        Ok(ModelResponse {
            quota: Vec::new(),
            text,
            tool_calls: calls,
            last_request_usage: None,
            provider_state: item.get("provider_state").cloned().filter(|v| !v.is_null()),
            usage: Some(Usage {
                input_tokens: usage(&payload)["input_tokens"].as_u64().unwrap_or_default(),
                output_tokens: usage(&payload)["output_tokens"]
                    .as_u64()
                    .unwrap_or_default(),
                cache_read_tokens: payload["usage"]["cache_read_tokens"]
                    .as_u64()
                    .unwrap_or_default(),
                cache_creation_tokens: payload["usage"]["cache_creation_tokens"]
                    .as_u64()
                    .unwrap_or_default(),
                reasoning_tokens: payload["usage"]["completion_tokens_details"]["reasoning_tokens"]
                    .as_u64(),
            }),
        })
    }
    async fn close_session(&self, session_id: &str) -> Result<()> {
        self.model.close_session(session_id).await
    }
}

pub async fn answer(
    model: Arc<dyn ProbeModel>,
    store: &Store,
    session: &str,
    frozen: &ModelRequest,
    probe: &Probe,
) -> ProbeAnswer {
    answer_with_policy(
        model,
        store,
        session,
        frozen,
        probe,
        crate::config::Agents::default(),
    )
    .await
}

/// Run a recall probe with its host's configured worker lifecycle policy.
pub async fn answer_with_policy(
    model: Arc<dyn ProbeModel>,
    store: &Store,
    session: &str,
    frozen: &ModelRequest,
    probe: &Probe,
    policy: crate::config::Agents,
) -> ProbeAnswer {
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = ProbeCancelOnDrop(cancellation.clone());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut tool_calls = Vec::new();
    let mut worker = None;
    let result: Result<String> = async {
        let model_name = model.model().to_owned();
        let effort = model.reasoning_effort();
        let mut source_tools = ToolRegistry::new();
        crate::tools::register_history_tools(&mut source_tools, store.clone())?;
        let mut registry = ToolRegistry::new();
        for name in ["history_search", "history_read"] {
            registry.register(Arc::new(ProbeHistoryTool {
                source: session.into(),
                tool: source_tools.get_host(name).context("History tool unavailable")?,
            }))?;
        }
        let harness = Arc::new(Harness::new(
            store.clone(),
            Arc::new(ProbeProvider { model, calls: calls.clone() }),
            registry,
            HarnessConfig {
                model: model_name.clone(),
                max_rounds: Some(4),
                context: Arc::new(FrozenProbeContext { frozen: frozen.clone(), effort }),
                ..Default::default()
            },
        ));
        let profile = crate::agents::AgentProfile {
            name: "recall-probe".into(),
            description: "Frozen recall probe".into(),
            harness,
            model: Some(model_name),
            max_rounds: Some(4),
            approval: None,
        };
        let agents = crate::agents::AgentTools::from_profiles(store.clone(), vec![profile.clone()])?
            .with_limits(policy);
        let context = ToolContext {
            session_id: session.into(),
            run_id: "recall-probe".into(),
            invocation_id: probe.id.clone(),
            cancellation: cancellation.clone(),
            approval: Default::default(),
        };
        let receipt = agents.start_worker(&context, profile, json!({"task":probe.question}))?;
        let address = receipt["sessionId"].as_str().context("Worker receipt lacks session")?.to_owned();
        let run_id = receipt["runId"].as_str().context("Worker receipt lacks run ID")?.to_owned();
        worker = Some((agents, session.to_owned(), address.clone(), run_id));
        let (agents, owner, _, _) = worker.as_ref().unwrap();
        let completed = agents.wait(owner, &address, None).await?;
        let history = store.messages(&address)?;
        let mut round = 0;
        for (index, message) in history.iter().enumerate() {
            if !message.tool_calls.is_empty() {
                round += 1;
                for call in &message.tool_calls {
                    if let Some(result) = history[index + 1..].iter().find(|m| m.tool_call_id.as_ref() == Some(&call.id)) {
                        tool_calls.push(json!({"round":round,"id":call.id,"name":call.name,"arguments":call.arguments,"is_error":result.is_error,"result":result.content}));
                    }
                }
            }
        }
        ensure!(completed["status"] == "completed", "Probe worker did not complete");
        history.into_iter().rev().find(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
            .map(|m| m.content).filter(|text| !text.is_empty()).context("Probe worker answer has no text")
    }.await;
    let calls = calls.lock().unwrap().clone();
    let worker_session_id = worker.as_ref().map(|(_, _, session, _)| session.clone());
    let worker_run_id = worker.as_ref().map(|(_, _, _, run)| run.clone());
    let value = match result {
        Ok(answer) => {
            json!({"status":"answered","answer":answer,"calls":calls,"tool_calls":tool_calls,"worker_session_id":worker_session_id,"worker_run_id":worker_run_id})
        }
        Err(_) => {
            json!({"status":"answer_failed","error":"Probe call, response or tool contract failure; no retry","calls":calls,"tool_calls":tool_calls,"worker_session_id":worker_session_id,"worker_run_id":worker_run_id})
        }
    };
    ProbeAnswer { value, worker }
}

#[path = "probe_judge.rs"]
mod judge_requests;
pub use judge_requests::judge;
pub(crate) use judge_requests::judge_for_session;

pub fn metadata() -> Value {
    metadata_for(MODEL, ReasoningEffort::Low)
}
pub(crate) fn metadata_for(model: &str, effort: ReasoningEffort) -> Value {
    json!({"answer_model":model,"judge_model":model,"reasoning_effort":effort.as_str(),"max_tool_rounds":3,"judge_prompt":"probe_judge_prompt_v1.txt","judge_prompt_sha256":format!("{:x}",Sha256::digest(JUDGE_PROMPT)),"answer_length_policy":"Guidance only; never reject, retry or truncate for length","tools":["history_search","history_read"],"view":"Cloned prepared request; frozen source view; invisible worker persists complete probe rounds until result acknowledgement"})
}
pub fn aggregate(rows: &[Value], strategy: &str) -> Vec<Value> {
    let mut groups = BTreeMap::<(u64, String), Value>::new();
    for row in rows {
        let checkpoint = row["checkpoint"].as_u64().unwrap_or_default();
        let kind = row["type"].as_str().unwrap_or("unknown").to_owned();
        let group = groups.entry((checkpoint,kind.clone())).or_insert_with(|| json!({"strategy":strategy,"checkpoint":checkpoint,"type":kind,"supported":0,"partial":0,"missing":0,"contradicted":0,"asked":0,"skipped":0,"answer_failed":0,"judge_failed":0,"with_tool_calls":0,"input_tokens":0,"output_tokens":0,"probe_cost_usd":0.0}));
        let status = row["status"].as_str().unwrap_or("skipped");
        if status == "skipped" {
            group["skipped"] = json!(group["skipped"].as_u64().unwrap() + 1);
            continue;
        }
        group["asked"] = json!(group["asked"].as_u64().unwrap() + 1);
        if status == "answer_failed" {
            group["answer_failed"] = json!(group["answer_failed"].as_u64().unwrap() + 1);
        }
        if row["judge"]["status"] == "judge_failed" {
            group["judge_failed"] = json!(group["judge_failed"].as_u64().unwrap() + 1);
        }
        if let Some(grade) = row["judge"]["judgment"]["grade"].as_str() {
            group[grade] = json!(group[grade].as_u64().unwrap() + 1);
        }
        if row["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty())
        {
            group["with_tool_calls"] = json!(group["with_tool_calls"].as_u64().unwrap() + 1);
        }
        for key in ["input_tokens", "output_tokens"] {
            group[key] =
                json!(group[key].as_u64().unwrap() + row["probe_usage"][key].as_u64().unwrap_or(0));
        }
        group["probe_cost_usd"] = json!(
            group["probe_cost_usd"].as_f64().unwrap()
                + row["probe_usage"]["cost_usd"].as_f64().unwrap_or(0.0)
        );
    }
    groups
        .into_values()
        .map(|mut group| {
            let judged = ["supported", "partial", "missing", "contradicted"]
                .iter()
                .map(|key| group[key].as_u64().unwrap())
                .sum::<u64>();
            group["judged"] = json!(judged);
            group["supported_recall"] = json!(if judged > 0 {
                Some(group["supported"].as_u64().unwrap() as f64 / judged as f64)
            } else {
                None
            });
            let asked = group["asked"].as_u64().unwrap();
            group["tool_call_rate"] = json!(if asked > 0 {
                Some(group["with_tool_calls"].as_u64().unwrap() as f64 / asked as f64)
            } else {
                None
            });
            group
        })
        .collect()
}
pub fn markdown(groups: &[Value]) -> String {
    let mut text = String::from(
        "## Recall probes\n\n| Strategy | Checkpoint | Type | Supported | Partial | Missing | Contradicted | Judged / asked | Skipped | Recall | Tool rate | Probe input / output | Probe USD |\n|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for g in groups {
        text.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} / {} | {} | {} | {} | {} / {} | {} |\n",
            g["strategy"].as_str().unwrap_or("?"),
            g["checkpoint"],
            g["type"].as_str().unwrap_or("?"),
            g["supported"],
            g["partial"],
            g["missing"],
            g["contradicted"],
            g["judged"],
            g["asked"],
            g["skipped"],
            g["supported_recall"],
            g["tool_call_rate"],
            g["input_tokens"],
            g["output_tokens"],
            g["probe_cost_usd"]
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    mod history_worker;
    use std::collections::VecDeque;

    fn probe() -> Probe {
        Probe {
            id: "test".into(),
            kind: ProbeType::Recall,
            question: "What colour?".into(),
            expected_answer: "The colour was blue.".into(),
            evidence_message_ids: vec!["r1".into()],
            earliest_checkpoint: 1,
            ask_at_turn: None,
            origin: None,
            original_question: None,
            source_message_id: None,
            source_turn: None,
        }
    }
    fn message(id: &str, session: &str) -> Message {
        Message {
            id: id.into(),
            session_id: session.into(),
            role: Role::User,
            origin: None,
            content: "Garden colour was blue".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-09T00:00:00Z".into(),
        }
    }
    struct FakeModel {
        replies: Mutex<VecDeque<Value>>,
        requests: Mutex<Vec<Value>>,
        closed: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl ProbeModel for FakeModel {
        async fn close_session(&self, session_id: &str) -> Result<()> {
            self.closed.lock().unwrap().push(session_id.into());
            Ok(())
        }
        async fn call(&self, body: Value, _kind: &str) -> Result<Value> {
            self.requests.lock().unwrap().push(body);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .context("No fake response")
        }
    }
    struct HangingModel {
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<tokio::sync::Notify>,
    }
    struct DroppedCall(Arc<tokio::sync::Notify>);
    impl Drop for DroppedCall {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    #[async_trait]
    impl ProbeModel for HangingModel {
        async fn call(&self, _body: Value, _kind: &str) -> Result<Value> {
            let _dropped = DroppedCall(self.dropped.clone());
            self.started.notify_one();
            std::future::pending().await
        }
    }
    fn response(message: Value) -> Value {
        json!({"choices":[{"message":message}],"usage":{"prompt_tokens":12,"completion_tokens":4}})
    }
    fn tool_response(name: &str, round: usize) -> Value {
        response(
            json!({"role":"assistant","content":null,"provider_state":{"opaque_signature":"probe-signature-byte-exact"},"tool_calls":[{"id":format!("read-id-{round}"),"type":"function","function":{"name":name,"arguments":"{\"message_id\":\"r1\"}"}}]}),
        )
    }
    #[tokio::test]
    async fn codec_preserves_generated_notice_signed_loop_errors_and_child_identity() -> Result<()>
    {
        use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
        use bubblepaw_core::{HistoryCursor, MessageOrigin};
        type Captured = Arc<Mutex<Vec<(String, Value)>>>;
        async fn respond(
            State(captured): State<Captured>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            captured
                .lock()
                .unwrap()
                .push((headers["x-opencode-session"].to_str().unwrap().into(), body));
            Json(
                json!({"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}),
            )
        }
        let captured: Captured = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let app = Router::new()
            .route("/messages", post(respond))
            .with_state(captured.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        struct StopServer(tokio::task::JoinHandle<std::io::Result<()>>);
        impl Drop for StopServer {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _server = StopServer(server);
        let mut assistant = message("assistant", "child");
        assistant.role = Role::Assistant;
        assistant.content.clear();
        assistant.tool_calls.push(ToolCall {
            id: "lookup".into(),
            name: "history_read".into(),
            arguments: json!({"message_id":"missing"}),
        });
        let signed = json!({"adapter":"anthropic-messages","content":[{"type":"thinking","thinking":"opaque thought","signature":"opaque+/=雪"},{"type":"tool_use","id":"lookup","name":"history_read","input":{"message_id":"missing"}}]});
        assistant.provider_state = Some(signed.clone());
        let mut result = message("result", "child");
        result.role = Role::Tool;
        result.tool_call_id = Some("lookup".into());
        result.is_error = true;
        let mut notice = message("notice", "child");
        notice.origin = Some(MessageOrigin::Generated);
        notice.content = "[host notice]\nSynthetic observation.".into();
        let messages = [message("user", "child"), assistant, result, notice];
        let cursor = HistoryCursor {
            epoch: "worker-epoch".into(),
            rewritten_from: Some("assistant".into()),
        };
        let mut body = request(&messages.iter().map(wire).collect::<Vec<_>>(), &[]);
        body["_bubblepaw_request"] = json!({"session_id":"child","history":cursor});
        let restored = provider_request(&body, "claude-haiku-5-5", ReasoningEffort::Low)?;
        assert_eq!(restored.session_id, "child");
        assert_eq!(restored.history, cursor);
        assert_eq!(restored.messages[1].id, "assistant");
        assert_eq!(restored.messages[1].provider_state, Some(signed));
        assert!(restored.messages[2].is_error);
        assert_eq!(restored.messages[3].origin, Some(MessageOrigin::Generated));
        let provider = crate::provider::MessagesProvider::api_key("synthetic-key", base)?
            .with_opencode_route();
        provider
            .generate(restored.clone(), Arc::new(|_| {}), CancellationToken::new())
            .await?;
        {
            let calls = captured.lock().unwrap();
            assert_eq!(calls[0].0, "child");
            assert_eq!(
                calls[0].1["messages"][1]["content"][0]["signature"],
                "opaque+/=雪"
            );
            assert_eq!(calls[0].1["messages"][2]["content"][0]["is_error"], true);
        }
        let mut missing = restored;
        missing.messages[1].provider_state = None;
        let error = provider
            .generate(missing, Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("current thinking tool loop"));
        assert_eq!(captured.lock().unwrap().len(), 1);
        Ok(())
    }
    #[test]
    fn eligibility_requires_all_evidence_behind_inclusive_frontier_and_checkpoint() {
        let history = vec![message("r1", "s"), message("r2", "s"), message("r3", "s")];
        let mut probe = probe();
        assert!(!eligible(&probe, 1, 7, &history, None));
        assert!(eligible(&probe, 1, 7, &history, Some("r1")));
        probe.evidence_message_ids.push("r3".into());
        assert!(!eligible(&probe, 1, 7, &history, Some("r2")));
        assert!(eligible(&probe, 1, 7, &history, Some("r3")));
        probe.earliest_checkpoint = 3;
        assert!(!eligible(&probe, 1, 7, &history, Some("r3")));
    }
    #[test]
    fn optional_position_and_provenance_preserve_legacy_probe_serialization() -> Result<()> {
        let legacy = json!({"id":"test","type":"recall","question":"What colour?","expected_answer":"The colour was blue.","evidence_message_ids":["r1"],"earliest_checkpoint":1});
        let parsed: Probe = serde_json::from_value(legacy.clone())?;
        assert_eq!(serde_json::to_value(parsed)?, legacy);
        assert_eq!(ask_positions(&[probe()]), Vec::<usize>::new());
        let mut cross_part = legacy;
        cross_part["type"] = json!("cross_part");
        assert!(matches!(
            serde_json::from_value::<Probe>(cross_part)?.kind,
            ProbeType::CrossPart
        ));
        Ok(())
    }
    #[test]
    fn natural_position_is_immediately_before_its_original_typed_question() -> Result<()> {
        let history = vec![message("r1", "s"), message("r2", "s"), message("r3", "s")];
        let mut natural = probe();
        natural.ask_at_turn = Some(1);
        natural.origin = Some(ProbeOrigin::Natural);
        natural.original_question = Some("What colour?".into());
        natural.source_message_id = Some("r2".into());
        natural.source_turn = Some(2);
        validate(std::slice::from_ref(&natural), &history)?;
        assert!(eligible(&natural, 1, 1, &history, Some("r1")));
        assert!(!eligible(&natural, 2, 2, &history, Some("r3")));
        natural.ask_at_turn = Some(2);
        assert!(validate(std::slice::from_ref(&natural), &history).is_err());
        natural.ask_at_turn = Some(1);
        natural.evidence_message_ids = vec!["r2".into()];
        assert!(validate(std::slice::from_ref(&natural), &history).is_err());
        natural.ask_at_turn = Some(0);
        natural.source_turn = Some(1);
        natural.source_message_id = Some("r1".into());
        assert!(validate(std::slice::from_ref(&natural), &history).is_err());
        Ok(())
    }
    #[test]
    fn positioned_probes_batch_once_and_require_inclusive_evidence_in_both_arms() -> Result<()> {
        let history: Vec<_> = ["r1", "r2", "r3", "r4"]
            .into_iter()
            .map(|id| message(id, "s"))
            .collect();
        let mut early = probe();
        early.ask_at_turn = Some(7);
        let mut same_position = early.clone();
        same_position.id = "same-position".into();
        let mut not_compacted_in_classical = early.clone();
        not_compacted_in_classical.id = "still-raw".into();
        not_compacted_in_classical.evidence_message_ids = vec!["r2".into()];
        let mut later = early.clone();
        later.id = "later".into();
        later.ask_at_turn = Some(12);
        later.evidence_message_ids = vec!["r2".into()];
        let mut frontiers = BTreeMap::new();
        for arm in STRATEGIES {
            frontiers.insert(arm.into(), vec![json!({"turn":7,"step":2,"end_message_id":"r2","frontier_message_id":if arm == "classical" { "r1" } else { "r2" },"status":"complete"}), json!({"turn":12,"step":4,"end_message_id":"r4","frontier_message_id":"r2","status":"complete"})]);
        }
        let keys = [later, early, same_position, not_compacted_in_classical];
        assert_eq!(ask_positions(&keys), vec![7, 12]);
        let checkpoints =
            common_checkpoints(&keys, &history, &ask_positions(&keys), &frontiers, None)?;
        assert_eq!(
            checkpoints[0].common_probe_ids,
            vec!["test", "same-position"]
        );
        assert_eq!(checkpoints[1].common_probe_ids, vec!["later"]);
        assert_eq!(
            checkpoints[0].skipped[0]["skip_reason"],
            "different_ask_turn"
        );
        assert_eq!(
            checkpoints[0].skipped[1]["uncovered_arms"],
            json!(["classical"])
        );
        assert_eq!(checkpoints[1].skipped.len(), 3);
        // Becoming compacted later never re-asks an earlier positioned question.
        assert!(
            checkpoints[1]
                .skipped
                .iter()
                .all(|row| row["skip_reason"] == "different_ask_turn")
        );
        frontiers.get_mut("classical").unwrap()[1]["frontier_message_id"] = Value::Null;
        let missing = common_checkpoints(&keys, &history, &[12], &frontiers, None)?;
        assert!(missing[0].common_probe_ids.is_empty());
        assert_eq!(
            missing[0].skipped[0]["uncovered_arms"],
            json!(["classical"])
        );
        Ok(())
    }
    #[tokio::test]
    async fn fourth_tool_round_and_nonhistory_tool_are_rejected() -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("fixture")?;
        for replies in [
            vec![tool_response("exec", 0)],
            (0..4)
                .map(|round| tool_response("history_read", round))
                .collect(),
        ] {
            let model = Arc::new(FakeModel {
                replies: Mutex::new(replies.into()),
                requests: Mutex::new(vec![]),
                closed: Mutex::new(Vec::new()),
            });
            let result = answer(
                model.clone(),
                &store,
                &session.id,
                &ModelRequest::default(),
                &probe(),
            )
            .await;
            assert_eq!(result["status"], "answer_failed");
            let worker_id = result.worker_session_id().unwrap();
            assert_eq!(result["worker_session_id"], worker_id);
            let run_id = result.worker_run_id().unwrap();
            assert_eq!(result["worker_run_id"], run_id);
            assert_eq!(store.runs(worker_id)?.last().unwrap().id, run_id);
            assert!(store.session(worker_id)?.hidden);
            assert!(model.requests.lock().unwrap().len() <= 4);
        }
        Ok(())
    }
    #[tokio::test]
    async fn configured_keep_all_retains_acknowledged_probe_worker() -> Result<()> {
        let store = Store::in_memory()?;
        let source = store.create_session("probe source")?;
        let model = Arc::new(FakeModel {
            replies: Mutex::new(VecDeque::from([response(
                json!({"role":"assistant","content":"Blue."}),
            )])),
            requests: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
        });
        let answer = answer_with_policy(
            model,
            &store,
            &source.id,
            &ModelRequest::default(),
            &probe(),
            crate::config::Agents {
                worker_keep_all: true,
                worker_retention_days: 9,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(answer["status"], "answered");
        let worker_id = answer.worker_session_id().unwrap().to_owned();
        answer.acknowledge().await?;
        assert!(store.session(&worker_id)?.hidden);
        assert_eq!(store.messages(&worker_id)?.last().unwrap().content, "Blue.");
        assert_eq!(store.runs(&worker_id)?.last().unwrap().status, "completed");
        Ok(())
    }

    #[tokio::test]
    async fn dropping_probe_answer_cancels_and_retains_its_worker() -> Result<()> {
        use bubblepaw_core::EventKind;
        use std::time::Duration;

        let store = Store::in_memory()?;
        let source = store.create_session("probe source")?;
        let harness = Harness::new(
            store.clone(),
            Arc::new(crate::provider::ScriptedProvider::new("must not call")),
            ToolRegistry::new(),
            HarnessConfig::default(),
        );
        let mut events = harness.subscribe();
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(tokio::sync::Notify::new());
        let model = Arc::new(HangingModel {
            started: started.clone(),
            dropped: dropped.clone(),
        });
        let task_store = store.clone();
        let source_id = source.id.clone();
        let task = tokio::spawn(async move {
            answer(
                model,
                &task_store,
                &source_id,
                &ModelRequest::default(),
                &probe(),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), started.notified()).await?;
        let worker = store
            .list_sessions()?
            .into_iter()
            .find(|session| session.hidden)
            .context("Probe did not create a hidden worker")?;
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        tokio::time::timeout(Duration::from_secs(5), dropped.notified()).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.recv().await?;
                if event.session_id == worker.id
                    && matches!(event.kind, EventKind::RunFinished { ref status, .. } if status == "cancelled")
                {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }).await??;
        assert!(store.session(&worker.id)?.hidden);
        assert_eq!(store.runs(&worker.id)?.last().unwrap().status, "cancelled");
        assert!(store.pending_effects(&worker.id)?.is_empty());
        assert!(store.messages(&source.id)?.is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn judge_parse_failure_is_retained_without_retry_or_grade() {
        let model = Arc::new(FakeModel {
            replies: Mutex::new(VecDeque::from([response(
                json!({"content":"I think supported"}),
            )])),
            requests: Mutex::new(vec![]),
            closed: Mutex::new(Vec::new()),
        });
        let result = judge(model.as_ref(), &probe(), "blue").await;
        assert_eq!(result["status"], "judge_failed");
        assert_eq!(result["raw"], "I think supported");
        assert!(result["judgment"].is_null());
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert!(
            parse_judgment("{\"grade\":\"supported\",\"reason\":\"fine\",\"extra\":1}").is_err()
        );
    }
    #[test]
    fn failed_judges_are_excluded_from_recall_denominator() {
        let rows = vec![
            json!({"type":"recall","checkpoint":1,"status":"answered","judge":{"status":"judge_failed"}}),
            json!({"type":"recall","checkpoint":1,"status":"answered","judge":{"status":"graded","judgment":{"grade":"supported"}}}),
        ];
        let groups = aggregate(&rows, "classical");
        assert_eq!(groups[0]["asked"], 2);
        assert_eq!(groups[0]["judged"], 1);
        assert_eq!(groups[0]["judge_failed"], 1);
        assert_eq!(groups[0]["supported_recall"], 1.0);
    }
    #[test]
    fn common_plan_uses_the_all_arm_intersection_and_position_list_ordinal() -> Result<()> {
        let history = vec![
            message("r1", "s"),
            message("r2", "s"),
            message("r3", "s"),
            message("r4", "s"),
        ];
        let p1 = probe();
        let mut p2 = probe();
        p2.id = "later".into();
        p2.evidence_message_ids = vec!["r2".into()];
        let mut p3 = probe();
        p3.id = "ordinal-two".into();
        p3.earliest_checkpoint = 2;
        let mut frontiers = BTreeMap::new();
        for (arm, end) in [("lcm", "r3"), ("classical", "r2")] {
            frontiers.insert(arm.into(),vec![json!({"turn":7,"step":2,"end_message_id":"r2","frontier_message_id":if arm == "classical" { "r1" } else { "r2" },"compaction_count":1,"status":"complete"}),json!({"turn":12,"step":4,"end_message_id":"r4","frontier_message_id":end,"compaction_count":5,"status":"complete"})]);
        }
        let keys = vec![p1, p2, p3];
        let checkpoints = common_checkpoints(&keys, &history, &[7, 12], &frontiers, None)?;
        assert_eq!(checkpoints[0].common_probe_ids, vec!["test"]);
        assert_eq!(
            checkpoints[1].common_probe_ids,
            vec!["test", "later", "ordinal-two"]
        );
        assert_eq!(
            checkpoints[0].skipped[0]["skip_reason"],
            "evidence_not_compacted_in_all_arms"
        );
        // LCM has r2 compacted, but the classical raw tail excludes it.
        assert_eq!(
            checkpoints[0].skipped[0]["uncovered_arms"],
            json!(["classical"])
        );
        assert_eq!(
            checkpoints[0].skipped[1]["skip_reason"],
            "before_earliest_checkpoint"
        );
        let limited = common_checkpoints(&keys, &history, &[12], &frontiers, Some(1))?;
        assert_eq!(limited[0].common_probe_ids, vec!["test"]);
        assert_eq!(limited[0].skipped[0]["skip_reason"], "probe_limit");
        assert_eq!(
            limited[0].skipped[1]["skip_reason"],
            "before_earliest_checkpoint"
        );
        Ok(())
    }
}
