//! Prompt-change log and unexplained cache-miss warnings. Every build opens one
//! row per lineage; the row is written once, to the serving view, when usage
//! arrives or the lineage's next request supersedes it. The frozen and cold
//! tiers are written by hash when they change, so any request can be rebuilt
//! from the row, those tiers and the already-stored messages.
use super::*;
use crate::{ModelResponse, ReasoningEffort, Store, Usage, store::usage::prompt_log::WeakStore};
use std::time::{Duration, Instant};

/// Two 64-token provider blocks; smaller shortfalls are cache granularity.
pub const MISS_TOLERANCE_TOKENS: u64 = 128;
/// Shortfalls within this share of the prediction are byte-ratio estimate error.
pub const MISS_TOLERANCE_PERCENT: u64 = 10;
/// Prefixes shorter than this are not cached (Anthropic's smallest minimum).
pub const MIN_CACHEABLE_TOKENS: u64 = 1024;
/// Anthropic's default ephemeral cache lifetime; no adapter requests 1 h.
pub const ANTHROPIC_CACHE_LIFETIME: Duration = Duration::from_secs(300);

/// One model request as logged in the serving view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestLogRow {
    pub session_id: String,
    pub lineage: String,
    pub seq: u64,
    pub created_at: String,
    pub frozen: TierFingerprint,
    pub cold: TierFingerprint,
    pub history: TierFingerprint,
    pub classification: String,
    pub declared: bool,
    pub declaration_reason: Option<String>,
    pub provider: String,
    pub model: String,
    pub cached_tokens: Option<u64>,
    pub uncached_tokens: Option<u64>,
    pub expected_cached_tokens: Option<u64>,
    pub since_previous_ms: Option<u64>,
    pub warned: bool,
    /// Cache writes (Anthropic `cache_creation_input_tokens`); None where the
    /// provider does not report them (zero from a provider without a known cache).
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    /// As requested; None is the adapter's default.
    pub reasoning_effort: Option<String>,
    /// Wire positions of the `cache_control` marks sent (`system.0`,
    /// `messages.4.1`); None for adapters that place none.
    pub breakpoints: Option<Vec<String>>,
    pub cache_lifetime_ms: Option<u64>,
    /// Gap since the previous request exceeded the lifetime; None when unknown.
    pub likely_expired: Option<bool>,
}

impl RequestLogRow {
    /// `provider_state` key an adapter reports its sent cache breakpoints under.
    pub const BREAKPOINTS_KEY: &'static str = "_bubblepaw_cache_breakpoints";
    /// The breakpoints an adapter reported on this response.
    pub fn reported_breakpoints(response: &ModelResponse) -> Option<Vec<String>> {
        let state = response
            .provider_state
            .as_ref()?
            .get(Self::BREAKPOINTS_KEY)?;
        serde_json::from_value(state.clone()).ok()
    }
    /// Remove the report so it is not persisted with the message.
    pub fn take_breakpoints(response: &mut ModelResponse) -> Option<Vec<String>> {
        let breakpoints = Self::reported_breakpoints(response);
        let state = response
            .provider_state
            .as_mut()
            .and_then(Value::as_object_mut);
        if let Some(state) = state
            && state.remove(Self::BREAKPOINTS_KEY).is_some()
            && state.is_empty()
        {
            response.provider_state = None;
        }
        breakpoints
    }
}

pub(super) struct Open {
    row: RequestLogRow,
    unchanged_bytes: usize,
    total_bytes: usize,
    tiers: Vec<(String, String)>,
}
pub(super) struct Closed {
    row: RequestLogRow,
    tiers: Vec<(String, String)>,
    warning: Option<Value>,
}
struct Lineage {
    seq: u64,
    at: Instant,
    frozen: String,
    cold: String,
    open: Option<Open>,
}
#[derive(Default)]
pub(super) struct Log {
    lineages: HashMap<(String, String), Lineage>,
    store: Option<WeakStore>,
}

/// What a build knows about its request, for the log.
pub(super) struct Opening<'a> {
    pub session: &'a str,
    pub lineage: &'a str,
    /// The lineage whose last request wrote the cache this one reads (a fork's parent).
    pub reads_from: &'a str,
    pub model: &'a str,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub unchanged_bytes: usize,
    pub total_bytes: usize,
}

impl Log {
    /// Open this build's row, returning a superseded row that never got usage.
    pub(super) fn open(
        &mut self,
        opening: Opening<'_>,
        record: &mut ContextBuild,
        contents: impl FnOnce(bool, bool) -> Vec<(String, String)>,
    ) -> Option<Closed> {
        let now = Instant::now();
        let reads_from = (opening.session.to_owned(), opening.reads_from.to_owned());
        let since = self
            .lineages
            .get(&reads_from)
            .map(|previous| now.duration_since(previous.at).as_millis() as u64);
        let key = (opening.session.to_owned(), opening.lineage.to_owned());
        let lineage = self.lineages.entry(key).or_insert_with(|| Lineage {
            seq: 0,
            at: now,
            frozen: String::new(),
            cold: String::new(),
            open: None,
        });
        let superseded = lineage.open.take().map(|open| close(open, None, None));
        lineage.seq += 1;
        lineage.at = now;
        // Unchanged tiers were written with an earlier row that is still retained.
        let tiers = contents(
            lineage.frozen != record.frozen.sha256,
            lineage.cold != record.cold.sha256,
        );
        lineage.frozen.clone_from(&record.frozen.sha256);
        lineage.cold.clone_from(&record.cold.sha256);
        record.seq = lineage.seq;
        record.since_previous_ms = since;
        lineage.open = Some(Open {
            row: RequestLogRow {
                session_id: opening.session.to_owned(),
                lineage: opening.lineage.to_owned(),
                seq: lineage.seq,
                created_at: crate::now(),
                frozen: record.frozen.clone(),
                cold: record.cold.clone(),
                history: record.history.clone(),
                classification: classification_name(&record.classification),
                declared: record.declared,
                declaration_reason: record.declaration_reason.clone(),
                model: opening.model.to_owned(),
                reasoning_effort: opening
                    .reasoning_effort
                    .map(|effort| effort.as_str().to_owned()),
                since_previous_ms: since,
                ..RequestLogRow::default()
            },
            unchanged_bytes: opening.unchanged_bytes,
            total_bytes: opening.total_bytes,
            tiers,
        });
        superseded
    }
    /// A side call's declaration is settled after the shared build.
    pub(super) fn redeclare(&mut self, session: &str, lineage: &str, reason: &str) {
        if let Some(open) = self
            .lineages
            .get_mut(&(session.to_owned(), lineage.to_owned()))
            .and_then(|lineage| lineage.open.as_mut())
        {
            open.row.declared = true;
            open.row.declaration_reason = Some(reason.to_owned());
        }
    }
    fn release(&mut self, session: Option<&str>) -> Vec<Closed> {
        let mut closed = Vec::new();
        self.lineages.retain(|(id, _), lineage| {
            if session.is_some_and(|session| session != id) {
                return true;
            }
            closed.extend(lineage.open.take().map(|open| close(open, None, None)));
            false
        });
        closed
    }
}

/// `cache_miss_unexplained` evidence, or None when the classification explains the usage.
fn close(
    open: Open,
    usage: Option<(&str, &str, &Usage)>,
    breakpoints: Option<Vec<String>>,
) -> Closed {
    let Open {
        mut row,
        unchanged_bytes,
        total_bytes,
        tiers,
    } = open;
    if let Some((provider, model, usage)) = usage {
        if !provider.is_empty() {
            row.provider = provider.to_owned();
        }
        if !model.is_empty() {
            row.model = model.to_owned();
        }
        row.cached_tokens = Some(usage.cache_read_tokens);
        let reports_writes = cache_lifetime(&row.provider, &row.model).is_some();
        row.cache_write_tokens = (reports_writes || usage.cache_creation_tokens > 0)
            .then_some(usage.cache_creation_tokens);
        row.output_tokens = Some(usage.output_tokens);
        row.reasoning_tokens = usage.reasoning_tokens;
        row.uncached_tokens = Some(usage.input_tokens.saturating_sub(usage.cache_read_tokens));
        row.expected_cached_tokens = Some(
            (u128::from(usage.input_tokens) * unchanged_bytes as u128 / total_bytes.max(1) as u128)
                as u64,
        );
    }
    row.breakpoints = breakpoints;
    row.cache_lifetime_ms =
        cache_lifetime(&row.provider, &row.model).map(|lifetime| lifetime.as_millis() as u64);
    row.likely_expired = row
        .cache_lifetime_ms
        .zip(row.since_previous_ms)
        .map(|(lifetime, since)| since > lifetime);
    let undeclared = !matches!(row.classification.as_str(), "first" | "append") && !row.declared;
    let shortfall = row.classification == "append"
        && row
            .expected_cached_tokens
            .zip(row.cached_tokens)
            .is_some_and(|(expected, cached)| {
                let tolerance = MISS_TOLERANCE_TOKENS.max(expected * MISS_TOLERANCE_PERCENT / 100);
                expected >= MIN_CACHEABLE_TOKENS && cached + tolerance < expected
            });
    let warning = (undeclared || shortfall).then(|| {
        row.warned = true;
        let (lifetime_ms, likely_expired) =
            (row.cache_lifetime_ms, row.likely_expired.unwrap_or(false));
        let cause = if undeclared {
            "undeclared_change"
        } else {
            "cached_shortfall"
        };
        tracing::warn!(
            "unexplained cache miss ({cause}) in session {} lineage {} seq {}: expected {:?} cached tokens, got {:?}; {:?} ms since the previous request{}",
            row.session_id,
            row.lineage,
            row.seq,
            row.expected_cached_tokens,
            row.cached_tokens,
            row.since_previous_ms,
            if likely_expired {
                ", likely expired"
            } else {
                ""
            },
        );
        json!({"session_id":row.session_id,"lineage":row.lineage,"seq":row.seq,"cause":cause,"provider":row.provider,"model":row.model,"classification":row.classification,"declared":row.declared,"declaration_reason":row.declaration_reason,"expected_cached_tokens":row.expected_cached_tokens,"cached_tokens":row.cached_tokens,"uncached_tokens":row.uncached_tokens,"since_previous_ms":row.since_previous_ms,"cache_lifetime_ms":lifetime_ms,"likely_expired":likely_expired})
    });
    Closed {
        row,
        tiers,
        warning,
    }
}

/// Known provider cache lifetimes. Anthropic's is 5 minutes unless 1 h is requested.
pub fn cache_lifetime(provider: &str, model: &str) -> Option<Duration> {
    (provider.contains("anthropic") || model.starts_with("claude"))
        .then_some(ANTHROPIC_CACHE_LIFETIME)
}

fn classification_name(classification: &ContextClassification) -> String {
    match classification {
        ContextClassification::First => "first",
        ContextClassification::Append => "append",
        ContextClassification::FrozenChange => "frozen_change",
        ContextClassification::ColdChange => "cold_change",
        ContextClassification::HistoryRewrite { .. } => "history_rewrite",
    }
    .to_owned()
}

/// Bytes of the prefix the provider should still hold: the unchanged tiers
/// and the history messages before the first difference.
pub(super) fn unchanged_bytes(
    classification: &ContextClassification,
    previous_history: usize,
    measured: &Measured,
) -> usize {
    let tiers = measured.frozen.bytes + measured.cold.bytes;
    let history = |count: usize| measured.sizes.iter().take(count).sum::<usize>();
    match classification {
        ContextClassification::First | ContextClassification::FrozenChange => 0,
        ContextClassification::ColdChange => measured.frozen.bytes,
        ContextClassification::Append => tiers + history(previous_history),
        ContextClassification::HistoryRewrite {
            first_differing_index,
        } => tiers + history(*first_differing_index),
    }
}

impl ContextState {
    /// Write rows to this store's serving view. The first attached store wins.
    pub fn attach_request_log(&self, store: &Store) {
        let mut state = self.state.lock().unwrap();
        if state.log.store.is_none() {
            state.log.store = Some(store.downgrade());
        }
    }
    /// Close the lineage's open row with the provider's usage and write it.
    /// Returns the `cache_miss_unexplained` metrics when the miss is unexplained.
    pub fn finish_request(
        &self,
        session: &str,
        lineage: &str,
        provider: &str,
        model: &str,
        usage: Option<&Usage>,
        breakpoints: Option<Vec<String>>,
    ) -> Option<Value> {
        let (closed, store) = {
            let mut state = self.state.lock().unwrap();
            let open = state
                .log
                .lineages
                .get_mut(&(session.to_owned(), lineage.to_owned()))?
                .open
                .take()?;
            let closed = close(
                open,
                usage.map(|usage| (provider, model, usage)),
                breakpoints,
            );
            (
                closed,
                state.log.store.as_ref().and_then(WeakStore::upgrade),
            )
        };
        let warning = closed.warning.clone();
        write(store, [closed]);
        warning
    }
    pub(super) fn write_log(&self, closed: impl IntoIterator<Item = Closed>) {
        let store = self
            .state
            .lock()
            .unwrap()
            .log
            .store
            .as_ref()
            .and_then(WeakStore::upgrade);
        write(store, closed);
    }
    pub(super) fn release_log(&self, session: &str) {
        let closed = self.state.lock().unwrap().log.release(Some(session));
        self.write_log(closed);
    }
}

impl Drop for ContextState {
    fn drop(&mut self) {
        let closed = self.state.get_mut().map(|state| state.log.release(None));
        if let Ok(closed) = closed {
            self.write_log(closed);
        }
    }
}

fn write(store: Option<Store>, closed: impl IntoIterator<Item = Closed>) {
    let Some(store) = store else {
        return;
    };
    for closed in closed {
        if let Err(error) = store.log_model_request(&closed.row, &closed.tiers) {
            tracing::warn!("failed to log model request: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, Role};

    fn request(system: &str, contents: &[&str]) -> ModelRequest {
        ModelRequest {
            model: "claude-test".into(),
            system_prompt: system.into(),
            messages: contents
                .iter()
                .map(|content| Message {
                    id: String::new(),
                    session_id: "s".into(),
                    role: Role::User,
                    origin: None,
                    content: content.repeat(200),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    provider_state: None,
                    is_error: false,
                    created_at: String::new(),
                })
                .collect(),
            ..ModelRequest::default()
        }
    }
    fn usage(input: u64, cached: u64) -> Usage {
        Usage {
            input_tokens: input,
            cache_read_tokens: cached,
            ..Usage::default()
        }
    }
    /// Build and finish one root request; returns its warning.
    fn call(
        context: &ContextState,
        system: &str,
        contents: &[&str],
        usage: Usage,
    ) -> Option<Value> {
        context.build(request(system, contents), "s", "root", &[]);
        context.finish_request("s", "root", "anthropic", "claude-test", Some(&usage), None)
    }

    #[test]
    fn a_fully_cached_append_gives_no_warning() {
        let context = ContextState::default();
        assert!(call(&context, "system", &["a", "b"], usage(20_000, 0)).is_none());
        // The appended message is a third of the history: about two thirds is expected.
        let warning = call(&context, "system", &["a", "b", "c"], usage(30_000, 19_900));
        assert!(warning.is_none(), "{warning:?}");
    }

    #[test]
    fn an_undeclared_frozen_change_gives_a_warning() {
        let context = ContextState::default();
        call(&context, "system", &["a"], usage(10_000, 0));
        let warning = call(&context, "changed", &["a", "b"], usage(20_000, 0)).unwrap();
        assert_eq!(warning["cause"], "undeclared_change");
        assert_eq!(warning["classification"], "frozen_change");
        assert_eq!(warning["lineage"], "root");
        assert_eq!(warning["seq"], 2);
        // A declared change is explained, whatever it costs.
        context.declare(Some("s"), "lcm_apply");
        assert!(call(&context, "again", &["a", "b", "c"], usage(30_000, 0)).is_none());
    }

    #[test]
    fn a_cached_shortfall_on_an_append_reports_the_gap() {
        let context = ContextState::default();
        call(&context, "system", &["a", "b"], usage(20_000, 0));
        let warning = call(&context, "system", &["a", "b", "c"], usage(30_000, 2_000)).unwrap();
        assert_eq!(warning["cause"], "cached_shortfall");
        assert_eq!(warning["classification"], "append");
        assert_eq!(warning["cached_tokens"], 2_000);
        assert!(warning["expected_cached_tokens"].as_u64().unwrap() > 15_000);
        assert!(warning["since_previous_ms"].is_u64());
        assert_eq!(warning["cache_lifetime_ms"], 300_000);
        assert_eq!(warning["likely_expired"], false);
    }

    #[test]
    fn a_gap_past_the_cache_lifetime_is_likely_expired() {
        let context = ContextState::default();
        call(&context, "system", &["a", "b"], usage(20_000, 0));
        let earlier = Instant::now()
            .checked_sub(ANTHROPIC_CACHE_LIFETIME + Duration::from_secs(1))
            .unwrap();
        let key = ("s".to_owned(), "root".to_owned());
        context
            .state
            .lock()
            .unwrap()
            .log
            .lineages
            .get_mut(&key)
            .unwrap()
            .at = earlier;
        let warning = call(&context, "system", &["a", "b", "c"], usage(30_000, 0)).unwrap();
        assert!(warning["since_previous_ms"].as_u64().unwrap() > 300_000);
        assert_eq!(warning["likely_expired"], true);
        // Without a known lifetime nothing is presumed expired.
        assert_eq!(cache_lifetime("opencode-go", "deepseek"), None);
    }
}
