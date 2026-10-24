//! Attempt metrics from what the harness already reports: the live event stream the
//! host subscribes to, the existing observation records (the same ones the JSONL trace
//! writes), stored messages and context rows. Nothing here decides acceptance.
use super::checks::CheckResult;
use super::diff::DiffStat;
use super::pricing::TokenClasses;
use super::report::{FinalReport, RecordedExec};
use bubblepaw_core::{
    EventKind, HarnessEvent, Message, Observation, ObservationMode, ObservationRecord,
    ObservationSink, Usage,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Records kept in memory per process; later ones are counted as dropped.
const RECORD_LIMIT: usize = 200_000;

/// An observation sink that keeps records for delegate metrics and forwards each
/// record to an optional inner sink, such as the user's JSONL trace.
pub struct Collector {
    records: Mutex<Vec<ObservationRecord>>,
    dropped: std::sync::atomic::AtomicU64,
    inner: Option<Observation>,
}
impl Collector {
    pub fn new(inner: Option<Observation>) -> Arc<Self> {
        Arc::new(Self {
            records: Mutex::new(Vec::new()),
            dropped: Default::default(),
            inner,
        })
    }
    /// The observation to give the harness. A forwarded trace keeps its own mode.
    pub fn observation(self: &Arc<Self>) -> Observation {
        Observation {
            sink: self.clone(),
            mode: self
                .inner
                .as_ref()
                .map_or(ObservationMode::Metadata, |o| o.mode),
            max_payload_bytes: self.inner.as_ref().map_or(0, |o| o.max_payload_bytes),
        }
    }
    /// Remove and return the records of one session collected so far.
    pub fn take(&self, session_id: &str) -> Vec<ObservationRecord> {
        let mut records = self.records.lock().unwrap();
        let (mine, rest) = std::mem::take(&mut *records)
            .into_iter()
            .partition(|r| r.scope.session_id == session_id);
        *records = rest;
        mine
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}
impl ObservationSink for Collector {
    fn record(&self, record: ObservationRecord) {
        if let Some(inner) = &self.inner {
            inner.sink.record(record.clone());
        }
        let mut records = self.records.lock().unwrap();
        if records.len() < RECORD_LIMIT {
            records.push(record);
        } else {
            self.dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Debug)]
struct Started {
    id: String,
    name: String,
    arguments: Value,
}

/// Tool activity seen on the live event stream during one attempt, including the
/// transient nested calls a `js` cell makes.
#[derive(Debug, Default)]
pub struct Observed {
    started: Vec<Started>,
    finished: HashMap<String, (Value, bool, DateTime<Utc>)>,
    /// Final events after which the stream has nothing more for this attempt.
    pub settled: bool,
    /// Events missed because the subscriber fell behind the broadcast buffer.
    pub lagged: u64,
}
impl Observed {
    pub fn push(&mut self, at: DateTime<Utc>, event: &HarnessEvent) {
        match &event.kind {
            EventKind::ToolStarted { call } => self.started.push(Started {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            }),
            EventKind::ToolFinished {
                call_id, result, ..
            } => {
                let content = serde_json::from_str(&result.content)
                    .unwrap_or_else(|_| Value::String(result.content.clone()));
                self.finished
                    .insert(call_id.clone(), (content, result.is_error, at));
            }
            EventKind::SessionSettled {} => self.settled = true,
            _ => {}
        }
    }

    /// Exec commands in call order, with exit codes from the exec result or a later
    /// poll or wait of the same handle.
    pub fn execs(&self) -> Vec<(RecordedExec, Option<DateTime<Utc>>)> {
        let mut execs: Vec<(RecordedExec, Option<DateTime<Utc>>)> = Vec::new();
        let mut handles = HashMap::<String, usize>::new();
        for call in &self.started {
            let finished = self.finished.get(&call.id);
            if call.name == "exec" {
                let Some(command) = command_text(&call.arguments) else {
                    continue;
                };
                let index = execs.len();
                execs.push((
                    RecordedExec {
                        command,
                        exit_code: None,
                    },
                    None,
                ));
                if let Some((content, _, at)) = finished {
                    if let Some(handle) = content.get("handle").and_then(Value::as_str) {
                        handles.insert(handle.to_owned(), index);
                    }
                    if let Some(code) = exit_code(content) {
                        execs[index] = (
                            RecordedExec {
                                exit_code: Some(code),
                                ..execs[index].0.clone()
                            },
                            Some(*at),
                        );
                    }
                }
            } else if matches!(call.name.as_str(), "poll" | "wait")
                && let Some((content, _, at)) = finished
            {
                let mut results = Vec::new();
                collect_handles(content, &mut results);
                for (handle, code) in results {
                    if let Some(&index) = handles.get(&handle) {
                        execs[index].0.exit_code = Some(code);
                        execs[index].1 = Some(*at);
                    }
                }
            }
        }
        execs
    }
}

fn command_text(arguments: &Value) -> Option<String> {
    match arguments.get("command")? {
        Value::String(command) => Some(command.clone()),
        Value::Object(template) => {
            let parts = template.get("parts")?.as_array()?;
            let values = template.get("values")?.as_array()?;
            let mut text = String::new();
            for (index, part) in parts.iter().enumerate() {
                text.push_str(part.as_str()?);
                if let Some(value) = values.get(index) {
                    text.push_str(value.as_str()?);
                }
            }
            Some(text)
        }
        _ => None,
    }
}
fn exit_code(content: &Value) -> Option<i64> {
    content.get("exitCode").and_then(Value::as_i64)
}
fn collect_handles(value: &Value, out: &mut Vec<(String, i64)>) {
    match value {
        Value::Object(map) => {
            if let (Some(handle), Some(code)) = (
                map.get("handle").and_then(Value::as_str),
                map.get("exitCode").and_then(Value::as_i64),
            ) {
                out.push((handle.to_owned(), code));
            }
            map.values().for_each(|v| collect_handles(v, out));
        }
        Value::Array(values) => values.iter().for_each(|v| collect_handles(v, out)),
        _ => {}
    }
}

/// Exec commands recorded for an attempt, for false-green detection.
pub fn recorded_execs(observed: &Observed) -> Vec<RecordedExec> {
    observed.execs().into_iter().map(|(exec, _)| exec).collect()
}

/// One provider request of the worker turn.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CallRow {
    pub seq: usize,
    pub model: String,
    pub provider_route: Option<String>,
    /// Not recorded by any provider today.
    pub account: Option<String>,
    pub fallback_reason: Option<String>,
    pub tokens: Option<TokenClasses>,
    pub ttft_ms: Option<u64>,
    pub generation_ms: Option<u64>,
    pub tokens_per_s: Option<f64>,
    /// ok, fallback, error or cancelled.
    pub outcome: String,
    /// Prompt-cache accounting; None when the call carried none.
    pub expected_cacheable_tokens: Option<u64>,
    pub unexplained_miss_tokens: Option<u64>,
    /// Main-loop prefix break reason, or None when the prefix extended.
    pub prefix_break: Option<String>,
    /// Serialized request body size, from the matching `model_started`.
    pub wire_bytes: Option<u64>,
    #[serde(skip)]
    pub at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Reliability {
    pub provider_errors: u64,
    /// Provider adapters retry internally without reporting a count.
    pub retries: Option<u64>,
    pub fallbacks: Option<u64>,
    /// Not distinguished from other provider errors in what is recorded.
    pub rate_limits: Option<u64>,
    pub timeouts: Option<u64>,
    pub unknown_effects: u64,
    pub no_final_report: bool,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct AttemptMetrics {
    pub call_count: usize,
    pub cache_hit_ratio: Option<f64>,
    pub tps: Vec<f64>,
    pub median_tps: Option<f64>,
    pub p90_tps: Option<f64>,
    pub model_ms: Option<u64>,
    pub tool_ms: Option<u64>,
    pub tool_calls_by_name: BTreeMap<String, u64>,
    pub tool_errors: u64,
    pub malformed_calls: u64,
    pub repeated_calls: u64,
    pub done_without_check: bool,
    pub tokens_to_first_pass: Option<u64>,
    pub ms_to_first_pass: Option<u64>,
    pub lcm_gathers: Option<u64>,
    /// Applies overwrite one context view row; rejected summaries are not recorded.
    pub lcm_applies: Option<u64>,
    pub lcm_rejected: Option<u64>,
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub reliability: Reliability,
    /// Observation records lost to the in-memory bound.
    pub dropped_records: u64,
    /// Main-loop prefix breaks the harness did not declare.
    pub undeclared_busts: u64,
    /// Sum of unexplained cache misses over the calls that reported one (nulls skipped);
    /// None when no call reported one.
    pub unexplained_miss_tokens: Option<u64>,
}

pub struct Input<'a> {
    pub started_at: DateTime<Utc>,
    pub messages: &'a [Message],
    pub observed: &'a Observed,
    pub records: &'a [ObservationRecord],
    pub dropped_records: u64,
    pub checks: &'a [CheckResult],
    pub assignment_checks: &'a [String],
    pub report: Option<&'a FinalReport>,
    pub diff: Option<&'a DiffStat>,
    pub model: &'a str,
    pub model_phase: Duration,
    pub attempt_tokens: &'a TokenClasses,
    pub new_episodes: Option<u64>,
    pub unknown_effects: u64,
}

pub struct Collected {
    pub attempt: AttemptMetrics,
    pub calls: Vec<CallRow>,
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * (sorted.len() - 1) as f64).round() as usize;
    Some(sorted[rank.min(sorted.len() - 1)])
}

fn same_command(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

fn route_origin(messages: &[Message]) -> Option<String> {
    let mut routes = messages.iter().filter_map(|m| {
        m.provider_state
            .as_ref()?
            .get("_bubblepaw_route_origin")?
            .get("provider")?
            .as_str()
            .map(str::to_owned)
    });
    let first = routes.next()?;
    routes.all(|r| r == first).then_some(first)
}

pub fn collect(input: Input<'_>) -> Collected {
    let records = input.records;
    let route = route_origin(input.messages);
    // Model calls, paired with their start record by request id.
    let mut models = HashMap::<String, String>::new();
    let mut wire_sizes = HashMap::<String, u64>::new();
    let mut calls = Vec::new();
    let mut tool_nested = HashMap::<String, bool>::new();
    let mut attempt = AttemptMetrics {
        dropped_records: input.dropped_records,
        ..Default::default()
    };
    let mut model_ms = None::<u64>;
    let mut tool_ms = None::<u64>;
    let mut fallbacks = 0u64;
    for record in records {
        let metrics = &record.metrics;
        let at = DateTime::parse_from_rfc3339(&record.created_at)
            .ok()
            .map(|t| t.with_timezone(&Utc));
        let elapsed = metrics.get("elapsed_ms").and_then(Value::as_u64);
        match record.kind.as_str() {
            "model_started" => {
                if let (Some(id), Some(model)) = (
                    &record.scope.model_request_id,
                    metrics.get("model").and_then(Value::as_str),
                ) {
                    models.insert(id.clone(), model.to_owned());
                }
                if let (Some(id), Some(bytes)) = (
                    &record.scope.model_request_id,
                    metrics.get("wire_bytes").and_then(Value::as_u64),
                ) {
                    wire_sizes.insert(id.clone(), bytes);
                }
            }
            "model_finished" | "model_failed" => {
                let model = record
                    .scope
                    .model_request_id
                    .as_ref()
                    .and_then(|id| models.get(id).cloned())
                    .unwrap_or_else(|| input.model.to_owned());
                let tokens = metrics
                    .get("usage")
                    .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok())
                    .map(|u| TokenClasses::from(&u));
                let ttft = metrics.get("first_text_ms").and_then(Value::as_u64);
                let generation = elapsed.map(|e| e.saturating_sub(ttft.unwrap_or(0)));
                let tps = match (tokens, generation) {
                    (Some(t), Some(g)) if g > 0 && t.output > 0 => {
                        Some(t.output as f64 * 1000.0 / g as f64)
                    }
                    _ => None,
                };
                let fallback = metrics.get("fallback_from").map(|from| {
                    let reasons = metrics
                        .get("fallback_reasons")
                        .cloned()
                        .unwrap_or(Value::Null);
                    format!("from {from}: {reasons}")
                });
                let outcome = if record.kind == "model_failed" {
                    match metrics.get("error_category").and_then(Value::as_str) {
                        Some("cancelled") => "cancelled",
                        _ => {
                            attempt.reliability.provider_errors += 1;
                            "error"
                        }
                    }
                } else if fallback.is_some() {
                    fallbacks += 1;
                    "fallback"
                } else {
                    "ok"
                };
                if let Some(e) = elapsed {
                    *model_ms.get_or_insert(0) += e;
                }
                // The cache group names the route that answered; the route origin is the fallback.
                let provider_route = metrics
                    .get("cache_group")
                    .filter(|g| g.is_object())
                    .and_then(|g| {
                        let provider = g.get("provider")?.as_str().filter(|p| !p.is_empty())?;
                        Some(format!("{provider}/{}", g.get("model")?.as_str()?))
                    })
                    .or_else(|| route.clone());
                calls.push(CallRow {
                    seq: calls.len(),
                    model,
                    provider_route,
                    account: None,
                    fallback_reason: fallback,
                    tokens,
                    ttft_ms: ttft,
                    generation_ms: generation,
                    tokens_per_s: tps,
                    outcome: outcome.to_owned(),
                    expected_cacheable_tokens: metrics
                        .get("expected_cacheable_tokens")
                        .and_then(Value::as_u64),
                    unexplained_miss_tokens: metrics
                        .get("unexplained_miss_tokens")
                        .and_then(Value::as_u64),
                    prefix_break: metrics
                        .get("prefix_break")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    wire_bytes: record
                        .scope
                        .model_request_id
                        .as_ref()
                        .and_then(|id| wire_sizes.get(id).copied()),
                    at,
                });
            }
            "cache_bust" => {
                if metrics.get("declared").and_then(Value::as_bool) == Some(false) {
                    attempt.undeclared_busts += 1;
                }
            }
            "tool_started" => {
                if let Some(name) = metrics.get("tool").and_then(Value::as_str) {
                    *attempt
                        .tool_calls_by_name
                        .entry(name.to_owned())
                        .or_default() += 1;
                }
                if let Some(id) = &record.scope.call_id {
                    let nested = metrics
                        .get("nested")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    tool_nested.insert(id.clone(), nested);
                }
            }
            "tool_finished" | "tool_failed" => {
                let nested = record
                    .scope
                    .call_id
                    .as_ref()
                    .and_then(|id| tool_nested.get(id))
                    .copied()
                    .unwrap_or(false);
                // Nested calls run inside a top-level js cell; count time once.
                if !nested && let Some(e) = elapsed {
                    *tool_ms.get_or_insert(0) += e;
                }
                if record.kind == "tool_failed" {
                    attempt.tool_errors += 1;
                    if metrics.get("error_category").and_then(Value::as_str)
                        == Some("dispatch_error")
                    {
                        attempt.malformed_calls += 1;
                    }
                } else if metrics.get("is_error").and_then(Value::as_bool) == Some(true) {
                    attempt.tool_errors += 1;
                }
            }
            _ => {}
        }
    }
    attempt.call_count = calls.len();
    let unexplained: Vec<u64> = calls
        .iter()
        .filter_map(|c| c.unexplained_miss_tokens)
        .collect();
    attempt.unexplained_miss_tokens = (!unexplained.is_empty()).then(|| unexplained.iter().sum());
    attempt.model_ms = model_ms;
    attempt.tool_ms = tool_ms;
    attempt.reliability.fallbacks = (!calls.is_empty()).then_some(fallbacks);
    let mut tps: Vec<f64> = calls.iter().filter_map(|c| c.tokens_per_s).collect();
    tps.sort_by(f64::total_cmp);
    attempt.median_tps = percentile(&tps, 0.5);
    attempt.p90_tps = percentile(&tps, 0.9);
    attempt.tps = tps;
    let tokens = input.attempt_tokens;
    attempt.cache_hit_ratio =
        (tokens.input() > 0).then(|| tokens.cache_read as f64 / tokens.input() as f64);

    // Repeated identical calls; polling the same handle is how waiting works.
    let mut seen = HashMap::<(String, String), u64>::new();
    for call in &input.observed.started {
        if matches!(call.name.as_str(), "poll" | "wait" | "jobs") {
            continue;
        }
        let count = seen
            .entry((call.name.clone(), call.arguments.to_string()))
            .or_default();
        if *count > 0 {
            attempt.repeated_calls += 1;
        }
        *count += 1;
    }

    let execs = input.observed.execs();
    let is_check = |command: &str| {
        if input.assignment_checks.is_empty() {
            true
        } else {
            input
                .assignment_checks
                .iter()
                .any(|check| same_command(check, command))
        }
    };
    attempt.done_without_check = input.report.is_some_and(|r| r.status == "done")
        && !execs.iter().any(|(exec, _)| is_check(&exec.command));

    // First passing check: a worker exec of an acceptance check, else the harness run.
    let worker_pass = if input.assignment_checks.is_empty() {
        None
    } else {
        execs
            .iter()
            .filter(|(exec, _)| exec.exit_code == Some(0) && is_check(&exec.command))
            .filter_map(|(_, at)| *at)
            .min()
    };
    if let Some(at) = worker_pass {
        attempt.ms_to_first_pass = Some((at - input.started_at).num_milliseconds().max(0) as u64);
        attempt.tokens_to_first_pass = calls.iter().all(|c| c.tokens.is_some()).then(|| {
            calls
                .iter()
                .filter(|c| c.at.is_none_or(|t| t <= at))
                .filter_map(|c| c.tokens.map(|t| t.input() + t.output))
                .sum()
        });
    } else if let Some(index) = input.checks.iter().position(CheckResult::passed) {
        let check_ms: u64 = input.checks[..=index].iter().map(|c| c.duration_ms).sum();
        attempt.ms_to_first_pass = Some(input.model_phase.as_millis() as u64 + check_ms);
        attempt.tokens_to_first_pass = Some(tokens.input() + tokens.output);
    }

    attempt.lcm_gathers = input.new_episodes;
    if let Some(diff) = input.diff {
        attempt.files_changed = Some(diff.files.len() as u64);
        attempt.lines_added = Some(diff.added);
        attempt.lines_removed = Some(diff.removed);
    }
    attempt.reliability.unknown_effects = input.unknown_effects;
    attempt.reliability.no_final_report = input.report.is_none();
    Collected { attempt, calls }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::{ObservationScope, ToolCall, ToolResult};
    use serde_json::json;

    fn event(kind: EventKind) -> HarnessEvent {
        HarnessEvent {
            sequence: 0,
            durable: false,
            origin: None,
            session_id: "s".into(),
            run_id: "r".into(),
            created_at: String::new(),
            kind,
        }
    }
    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }
    fn started(observed: &mut Observed, t: i64, id: &str, name: &str, arguments: Value) {
        observed.push(
            at(t),
            &event(EventKind::ToolStarted {
                call: ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                },
            }),
        );
    }
    fn finished(observed: &mut Observed, t: i64, id: &str, content: Value, is_error: bool) {
        observed.push(
            at(t),
            &event(EventKind::ToolFinished {
                call_id: id.into(),
                name: String::new(),
                result: ToolResult {
                    content: content.to_string(),
                    is_error,
                    pending: false,
                },
            }),
        );
    }
    fn record(
        t: i64,
        kind: &str,
        request: Option<&str>,
        call: Option<&str>,
        metrics: Value,
    ) -> ObservationRecord {
        ObservationRecord {
            created_at: at(t).to_rfc3339(),
            kind: kind.into(),
            scope: ObservationScope {
                session_id: "s".into(),
                run_id: "r".into(),
                model_request_id: request.map(str::to_owned),
                call_id: call.map(str::to_owned),
                ..Default::default()
            },
            metrics,
            payload: None,
            payload_truncated: false,
        }
    }

    #[test]
    fn process_signals_and_call_rows_come_from_recorded_facts() {
        let mut observed = Observed::default();
        started(&mut observed, 1, "js1", "js", json!({"code":"a"}));
        started(
            &mut observed,
            1,
            "e1",
            "exec",
            json!({"command":"cargo test"}),
        );
        finished(
            &mut observed,
            2,
            "e1",
            json!({"status":"exited","exitCode":101}),
            false,
        );
        started(
            &mut observed,
            3,
            "e2",
            "exec",
            json!({"command":"cargo  test"}),
        );
        finished(
            &mut observed,
            4,
            "e2",
            json!({"status":"running","handle":"h1"}),
            false,
        );
        started(&mut observed, 5, "w1", "wait", json!({"handle":"h1"}));
        finished(
            &mut observed,
            9,
            "w1",
            json!({"handle":"h1","status":"exited","exitCode":0}),
            false,
        );
        started(&mut observed, 10, "r1", "read", json!({"path":"a.rs"}));
        started(&mut observed, 11, "r2", "read", json!({"path":"a.rs"}));
        started(&mut observed, 12, "w2", "wait", json!({"handle":"h1"}));
        let execs = observed.execs();
        assert_eq!(execs.len(), 2);
        assert_eq!(execs[0].0.exit_code, Some(101));
        assert_eq!(execs[1].0.exit_code, Some(0));
        assert_eq!(execs[1].1, Some(at(9)));

        let usage = |input: u64, read: u64, output: u64| json!({"input_tokens":input,"cache_read_tokens":read,"output_tokens":output});
        let records = vec![
            record(
                0,
                "model_started",
                Some("m1"),
                None,
                json!({"model":"opencode/deepseek-v4.1-flash","wire_bytes":4096}),
            ),
            record(
                1,
                "model_finished",
                Some("m1"),
                None,
                json!({"elapsed_ms":1000,"first_text_ms":200,"usage":usage(100, 50, 400),"cache_group":{"provider":"opencode-go","model":"deepseek-v4.1-flash","kind":"main"},"expected_cacheable_tokens":64,"measured_cache_read_tokens":50,"unexplained_miss_tokens":14,"prefix_break":"undeclared"}),
            ),
            // An undeclared main-loop break: counted; the declared one below is not.
            record(
                1,
                "cache_bust",
                Some("m1"),
                None,
                json!({"provider":"opencode-go","model":"deepseek-v4.1-flash","declared":false,"reason":"undeclared","expected_cacheable_tokens":64,"expected_uncached_tokens":36}),
            ),
            record(
                1,
                "tool_started",
                None,
                Some("js1"),
                json!({"tool":"js","nested":false}),
            ),
            record(
                1,
                "tool_started",
                None,
                Some("e1"),
                json!({"tool":"exec","nested":true}),
            ),
            record(
                2,
                "tool_finished",
                None,
                Some("e1"),
                json!({"elapsed_ms":900,"is_error":false}),
            ),
            record(
                9,
                "tool_finished",
                None,
                Some("js1"),
                json!({"elapsed_ms":8000,"is_error":true}),
            ),
            record(
                9,
                "tool_failed",
                None,
                Some("x"),
                json!({"elapsed_ms":1,"error_category":"dispatch_error"}),
            ),
            record(
                9,
                "model_started",
                Some("m2"),
                None,
                json!({"model":"opencode/deepseek-v4.1-flash"}),
            ),
            record(
                10,
                "model_failed",
                Some("m2"),
                None,
                json!({"elapsed_ms":300,"error_category":"provider_error"}),
            ),
            record(
                11,
                "model_started",
                Some("m3"),
                None,
                json!({"model":"opencode/deepseek-v4.1-flash"}),
            ),
            record(
                12,
                "model_finished",
                Some("m3"),
                None,
                json!({"elapsed_ms":500,"usage":usage(300, 250, 100),"fallback_from":"a/b","fallback_reasons":["rate_limited"],"cache_group":{"provider":"opencode-go","model":"deepseek-v4.1-flash","kind":"main"},"expected_cacheable_tokens":null,"measured_cache_read_tokens":250,"unexplained_miss_tokens":null,"prefix_break":"lcm_apply"}),
            ),
            record(
                12,
                "cache_bust",
                Some("m3"),
                None,
                json!({"provider":"opencode-go","model":"deepseek-v4.1-flash","declared":true,"reason":"lcm_apply","expected_cacheable_tokens":null,"expected_uncached_tokens":null}),
            ),
        ];
        let report = FinalReport {
            status: "done".into(),
            summary: String::new(),
            checks_claimed: vec![],
            open_questions: vec![],
        };
        let tokens = TokenClasses {
            uncached_input: 100,
            cache_read: 300,
            cache_write: 0,
            output: 500,
            reasoning: None,
        };
        let checks = ["cargo test".to_owned()];
        let collected = collect(Input {
            started_at: at(0),
            messages: &[],
            observed: &observed,
            records: &records,
            dropped_records: 0,
            checks: &[],
            assignment_checks: &checks,
            report: Some(&report),
            diff: Some(&DiffStat {
                files: vec!["a.rs".into()],
                added: 3,
                removed: 1,
            }),
            model: "opencode/deepseek-v4.1-flash",
            model_phase: Duration::from_secs(12),
            attempt_tokens: &tokens,
            new_episodes: Some(0),
            unknown_effects: 0,
        });
        let m = &collected.attempt;
        assert_eq!(collected.calls.len(), 3);
        let first = &collected.calls[0];
        assert_eq!((first.ttft_ms, first.generation_ms), (Some(200), Some(800)));
        assert_eq!(first.tokens_per_s, Some(500.0));
        assert_eq!(collected.calls[1].outcome, "error");
        assert_eq!(collected.calls[1].tokens, None);
        assert_eq!(collected.calls[2].outcome, "fallback");
        assert!(
            collected.calls[2]
                .fallback_reason
                .as_deref()
                .unwrap()
                .contains("rate_limited")
        );
        assert_eq!(m.call_count, 3);
        assert_eq!(m.model_ms, Some(1800));
        assert_eq!(
            m.tool_ms,
            Some(8001),
            "nested exec time is inside the js cell"
        );
        assert_eq!(m.tool_calls_by_name["exec"], 1);
        assert_eq!((m.tool_errors, m.malformed_calls), (2, 1));
        assert_eq!(
            m.repeated_calls, 1,
            "the second identical read; waits are excluded"
        );
        assert!(!m.done_without_check);
        assert_eq!(m.ms_to_first_pass, Some(9000));
        assert_eq!(m.tokens_to_first_pass, None, "a failed call has no usage");
        assert_eq!(m.cache_hit_ratio, Some(0.75));
        assert_eq!(m.median_tps, Some(500.0));
        assert_eq!(m.reliability.provider_errors, 1);
        assert_eq!(m.reliability.fallbacks, Some(1));
        assert_eq!(m.reliability.retries, None);
        assert_eq!(
            (m.files_changed, m.lines_added, m.lines_removed),
            (Some(1), Some(3), Some(1))
        );
        assert!(!m.reliability.no_final_report);
        // Cache fields: the route comes from the cache group, wire size from model_started.
        assert_eq!(
            first.provider_route.as_deref(),
            Some("opencode-go/deepseek-v4.1-flash")
        );
        assert_eq!(first.expected_cacheable_tokens, Some(64));
        assert_eq!(first.unexplained_miss_tokens, Some(14));
        assert_eq!(first.prefix_break.as_deref(), Some("undeclared"));
        assert_eq!(first.wire_bytes, Some(4096));
        assert_eq!(
            collected.calls[1].provider_route, None,
            "no cache group, no route origin"
        );
        assert_eq!(collected.calls[1].wire_bytes, None);
        assert_eq!(
            collected.calls[2].unexplained_miss_tokens, None,
            "a null stays null"
        );
        assert_eq!(
            collected.calls[2].prefix_break.as_deref(),
            Some("lcm_apply")
        );
        // One undeclared bust; the declared one is not counted. Nulls are skipped in the sum.
        assert_eq!(m.undeclared_busts, 1);
        assert_eq!(m.unexplained_miss_tokens, Some(14));

        // Done with no check command run, and no report at all.
        let mut quiet = Observed::default();
        started(&mut quiet, 1, "r", "read", json!({"path":"x"}));
        let base = |observed, report| Input {
            started_at: at(0),
            messages: &[],
            observed,
            records: &[],
            dropped_records: 0,
            checks: &[],
            assignment_checks: &checks,
            report,
            diff: None,
            model: "m/x",
            model_phase: Duration::ZERO,
            attempt_tokens: &tokens,
            new_episodes: None,
            unknown_effects: 2,
        };
        let done = collect(base(&quiet, Some(&report))).attempt;
        assert!(done.done_without_check);
        assert_eq!(done.reliability.unknown_effects, 2);
        assert_eq!(done.model_ms, None);
        let silent = collect(base(&quiet, None)).attempt;
        assert!(silent.reliability.no_final_report && !silent.done_without_check);
    }
}
