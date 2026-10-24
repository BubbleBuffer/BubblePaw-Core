//! Host usage projections over retained telemetry; never reads or rewrites history.
use crate::{
    config::{EffectiveConfig, Settings},
    pricing::{PriceTable, instant},
    summarization::{LcmConfig, SummaryConfig, cached_context_window},
};
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    ContextPreparation, Message, ModelRequest, ModelResponse, PrepareCall, PrepareMode,
    PreparedContext, Provider, Store, Usage,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

const LAST_REQUEST_KEY: &str = "usage.last_request";

#[derive(Clone, Debug)]
pub struct UsageSettings {
    pub default_model: String,
    pub default_provider: String,
    pub context_window_tokens: Option<usize>,
    pub compact_threshold_tokens: usize,
    pub lcm: LcmConfig,
    pub auto_summarize: bool,
    pub codex_models_cache: Option<PathBuf>,
}
impl Default for UsageSettings {
    fn default() -> Self {
        Self {
            default_model: String::new(),
            default_provider: String::new(),
            context_window_tokens: None,
            compact_threshold_tokens: 200_000,
            lcm: LcmConfig::default(),
            auto_summarize: true,
            codex_models_cache: None,
        }
    }
}
impl UsageSettings {
    /// Reads no process environment. The Codex cache path is set by the caller,
    /// normally from [`UsageSettings::from_effective`].
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            default_model: settings.main_model.clone(),
            default_provider: settings.main_provider.clone(),
            context_window_tokens: settings.context_window_tokens,
            compact_threshold_tokens: settings.compact_threshold_tokens,
            lcm: settings.lcm.clone(),
            auto_summarize: settings.auto_summarize,
            codex_models_cache: None,
        }
    }
    /// Settings plus the Codex cache path resolved from the configuration's own environment.
    pub fn from_effective(effective: &EffectiveConfig) -> Result<Self> {
        let mut usage = Self::from_settings(&effective.settings()?);
        usage.codex_models_cache = effective.codex_models_cache();
        Ok(usage)
    }
    fn window(&self, model: &str) -> Option<usize> {
        self.context_window_tokens
            .filter(|window| *window > 0)
            .or_else(|| {
                let (provider, model) = model
                    .split_once('/')
                    .unwrap_or((&self.default_provider, model));
                (provider == "codex")
                    .then(|| {
                        self.codex_models_cache
                            .as_ref()
                            .and_then(|path| cached_context_window(path, model))
                    })
                    .flatten()
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LastRequest {
    model: String,
    input_tokens: Option<u64>,
    observed_at: String,
}

/// Wrap once, after selecting the context policy, so ordinary and summarized history both report requests.
/// One bounded latest-value row and one change event are written per completed response.
pub struct UsageContext {
    inner: Arc<dyn ContextPreparation>,
    store: Store,
    models: Mutex<HashMap<String, (String, String)>>,
}
impl UsageContext {
    pub fn new(store: Store, inner: Arc<dyn ContextPreparation>) -> Self {
        Self {
            inner,
            store,
            models: Mutex::new(HashMap::new()),
        }
    }
    fn record_prepared(&self, session: &str, prepared: PreparedContext) -> Result<PreparedContext> {
        let run = self
            .store
            .snapshot(session, 0, 1, 0)?
            .runs
            .into_iter()
            .find(|run| run.status == "running");
        if let Some(run) = run {
            let mut models = self.models.lock().unwrap();
            // Failed/cancelled requests have no response callback. Bound their transient identities.
            if models.len() >= 1024
                && let Some(old) = models.keys().next().cloned()
            {
                models.remove(&old);
            }
            models.insert(session.into(), (prepared.request.model.clone(), run.id));
        }
        Ok(prepared)
    }
}
#[async_trait]
impl ContextPreparation for UsageContext {
    fn cold_message_indexes(&self, session: &str, request: &ModelRequest) -> Result<Vec<usize>> {
        self.inner.cold_message_indexes(session, request)
    }
    fn validate_prepared_request(
        &self,
        session: &str,
        request: &ModelRequest,
        provider: Arc<dyn Provider>,
    ) -> Result<()> {
        self.inner
            .validate_prepared_request(session, request, provider)
    }
    fn project_history(&self, history: &[Message]) -> Result<Vec<Message>> {
        self.inner.project_history(history)
    }
    fn history_frontier(&self, session: &str) -> Result<Option<i64>> {
        self.inner.history_frontier(session)
    }
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let session = call.session_id.to_owned();
        let is_request = !matches!(call.mode, PrepareMode::Compact { .. });
        let prepared = self.inner.prepare(call).await?;
        // Only the main request is recorded; manual compaction is not.
        if is_request {
            self.record_prepared(&session, prepared)
        } else {
            Ok(prepared)
        }
    }
    fn observe_request(&self, session: &str, request: &ModelRequest, provider: Arc<dyn Provider>) {
        self.inner.observe_request(session, request, provider);
    }
    fn take_prefix_change(&self, session_id: &str) -> Option<String> {
        self.inner.take_prefix_change(session_id)
    }
    fn release_session(&self, session: &str) {
        self.inner.release_session(session);
        self.models.lock().unwrap().remove(session);
    }
    fn observe_response(&self, session: &str, response: &ModelResponse) {
        self.inner.observe_response(session, response);
        let Some((model, run_id)) = self.models.lock().unwrap().remove(session) else {
            return;
        };
        let input_tokens = response
            .last_request_usage
            .as_ref()
            .map(|last| last.input_tokens)
            .unwrap_or_else(|| response.usage.as_ref().map(|usage| usage.input_tokens));
        let value = LastRequest {
            model,
            input_tokens,
            observed_at: chrono::Utc::now().to_rfc3339(),
        };
        // Reporting must not fail an accepted model response. A failed write leaves older telemetry.
        if let Err(error) = self
            .store
            .update_host_state(session, &run_id, LAST_REQUEST_KEY, |_| {
                Ok(serde_json::to_value(value)?)
            })
        {
            tracing::warn!(%error, "Could not record latest model usage");
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextFill {
    pub model: String,
    pub last_request_input_tokens: Option<u64>,
    pub last_request_observed_at: Option<String>,
    pub window_tokens: Option<usize>,
    pub fill_ratio: Option<f64>,
    pub compaction_trigger_tokens: usize,
    pub auto_compaction_enabled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub tokens: Usage,
    pub recorded_entries: u64,
    pub entries_without_usage: u64,
    pub estimated_cost_usd: Option<f64>,
    pub entries_without_pricing: u64,
    /// Retention, failed requests and older missing telemetry prevent an all-time billing claim.
    pub partial: bool,
}
impl UsageTotals {
    fn record(
        &mut self,
        model: &str,
        usage: Option<&Usage>,
        at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        self.partial = true;
        let Some(usage) = usage else {
            self.entries_without_usage += 1;
            return;
        };
        self.recorded_entries += 1;
        self.tokens.add(usage);
        let cost = PriceTable::shared().usage_cost(model, usage, at);
        if let Some(cost) = cost {
            self.estimated_cost_usd = Some(self.estimated_cost_usd.unwrap_or(0.0) + cost);
        } else {
            self.entries_without_pricing += 1;
        }
    }
    fn add(&mut self, other: &Self) {
        self.tokens.add(&other.tokens);
        self.recorded_entries += other.recorded_entries;
        self.entries_without_usage += other.entries_without_usage;
        self.entries_without_pricing += other.entries_without_pricing;
        if let Some(cost) = other.estimated_cost_usd {
            self.estimated_cost_usd = Some(self.estimated_cost_usd.unwrap_or(0.0) + cost);
        }
        self.partial = true;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatUsage {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub context: ContextFill,
    pub own: UsageTotals,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionUsage {
    pub session_id: String,
    pub context: ContextFill,
    pub own: UsageTotals,
    pub total: UsageTotals,
    /// Every descendant exactly once, including hidden internal workers. Each row is own usage only.
    pub descendants: Vec<ChatUsage>,
    /// Archived/archiving descendant storage is never restored by reporting.
    pub unavailable_session_ids: Vec<String>,
    pub basis: String,
}

fn own_usage(store: &Store, session: &str, settings: &UsageSettings) -> Result<ChatUsage> {
    let record = store.active_session_metadata(session)?.ok_or_else(|| {
        anyhow::anyhow!("Session usage unavailable while storage is archived or missing")
    })?;
    let runs = store.runs(session)?;
    let last: Option<LastRequest> = store
        .host_state(session, LAST_REQUEST_KEY)?
        .map(serde_json::from_value)
        .transpose()?;
    let model = last
        .as_ref()
        .map(|last| last.model.clone())
        .or_else(|| runs.last().map(|run| run.model.clone()))
        .unwrap_or_else(|| settings.default_model.clone());
    let window = settings.window(&model);
    let input = last.as_ref().and_then(|last| last.input_tokens);
    let context = ContextFill {
        model,
        last_request_input_tokens: input,
        last_request_observed_at: last.map(|last| last.observed_at),
        window_tokens: window,
        fill_ratio: input
            .zip(window)
            .map(|(input, window)| input as f64 / window as f64),
        compaction_trigger_tokens: SummaryConfig {
            context_window: window,
            threshold_cap: if record.parent_session_id.is_some() {
                settings
                    .lcm
                    .child_compact_threshold_tokens
                    .map(|child| child.min(settings.compact_threshold_tokens))
                    .unwrap_or(settings.compact_threshold_tokens)
            } else {
                settings.compact_threshold_tokens
            },
            lcm: settings.lcm.clone(),
            ..SummaryConfig::default()
        }
        .threshold(),
        auto_compaction_enabled: settings.auto_summarize,
    };
    let mut own = UsageTotals {
        partial: true,
        ..Default::default()
    };
    // Each record is priced at its own time; subject chapters store no time, so
    // their rate is the table's unknown-time rate (peak where a model has peak hours).
    for run in &runs {
        if run.model != bubblepaw_core::SCRIPT_MODEL {
            own.record(&run.model, run.usage.as_ref(), instant(&run.started_at));
        }
    }
    // Runner context policies return no preparation usage in runs; retained summary records are separate calls.
    for episode in store.context_episodes(session)? {
        own.record(
            &episode.model,
            episode.usage.as_ref(),
            instant(&episode.created_at),
        );
    }
    for chapter in store.context_chapters(session)? {
        own.record(
            &chapter.model,
            chapter.usage.as_ref(),
            instant(&chapter.created_at),
        );
    }
    for chapter in store.context_subject_chapters(session)? {
        if let Some(model) = chapter.model {
            own.record(&model, chapter.usage.as_ref(), None);
        }
    }
    for decision in store.context_decisions(session)? {
        own.record(
            &decision.model,
            decision.usage.as_ref(),
            instant(&decision.created_at),
        );
    }
    if let Some(checkpoint) = store.context_checkpoint(session)? {
        own.record(
            &checkpoint.model,
            checkpoint.usage.as_ref(),
            instant(&checkpoint.created_at),
        );
    }
    Ok(ChatUsage {
        session_id: session.into(),
        parent_session_id: record.parent_session_id,
        context,
        own,
    })
}

/// Read-only retained-telemetry projection. It never calls a provider or reconstructs usage from text.
pub fn session_usage(
    store: &Store,
    session: &str,
    settings: &UsageSettings,
) -> Result<SessionUsage> {
    let root = own_usage(store, session, settings)?;
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for record in store.list_sessions()? {
        if let Some(parent) = record.parent_session_id {
            children.entry(parent).or_default().push(record.id);
        }
    }
    let mut seen = HashSet::from([session.to_owned()]);
    let mut pending = children.remove(session).unwrap_or_default();
    let mut descendants = Vec::new();
    let mut unavailable_session_ids = Vec::new();
    let mut total = root.own.clone();
    while let Some(child) = pending.pop() {
        if !seen.insert(child.clone()) {
            continue;
        }
        pending.extend(children.remove(&child).unwrap_or_default());
        if store.active_session_metadata(&child)?.is_none() {
            unavailable_session_ids.push(child);
            continue;
        }
        let usage = own_usage(store, &child, settings)?;
        total.add(&usage.own);
        descendants.push(usage);
    }
    descendants.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    unavailable_session_ids.sort();
    Ok(SessionUsage {
        session_id: session.into(),
        context: root.context,
        own: root.own,
        total,
        descendants,
        unavailable_session_ids,
        basis: "retained recorded telemetry; partial estimates, not an all-time bill".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::UsageSettings;
    use crate::config::{EffectiveConfig, LoadOptions};

    #[test]
    fn usage_cache_path_comes_from_the_configuration_environment() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("configured-codex");
        let options = LoadOptions {
            workspace: root.path().join("workspace"),
            home: Some(root.path().join("home")),
            custom_file: None,
            env: vec![("CODEX_HOME".into(), codex_home.display().to_string())],
            cli: vec![],
        };
        let config = EffectiveConfig::load(options).unwrap();
        let usage = UsageSettings::from_effective(&config).unwrap();
        assert_eq!(
            usage.codex_models_cache,
            Some(codex_home.join("models_cache.json"))
        );
        assert_eq!(usage.codex_models_cache, config.codex_models_cache());
        // The settings-only constructor reads no process environment.
        assert_eq!(
            UsageSettings::from_settings(&config.settings().unwrap()).codex_models_cache,
            None
        );
    }
}
