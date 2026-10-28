//! Qualified `provider/model` routes share one harness provider; unqualified models use the default.
//! Explicit missing prefixes fail at dispatch instead of falling back.
use super::{RetryPolicy, metering::record_model_result, retry::add_usage, with_retry};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Capabilities, CursorResets, DecisionProvider, DecisionRequest,
    DecisionResponse, DecisionUsage, LastRequestUsage, ModelInfo, ModelRequest, ModelResponse,
    Provider, ProviderError, ProviderErrorKind, QuotaReading, RequestKind, RequestUsage, Store,
    TextCallback, Usage,
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(feature = "bench")]
#[path = "registry_bench.rs"]
mod benchmark;
#[path = "registry_context.rs"]
mod context_binding;
pub struct ProviderRegistry {
    providers: Vec<(String, Arc<dyn Provider>)>,
    decisions: Vec<(String, Arc<dyn DecisionProvider>)>,
    default: String,
    last: Mutex<HashMap<String, String>>,
    resets: CursorResets,
    origins: HashMap<String, String>,
    configured_models: HashMap<String, BTreeSet<String>>,
    removed_models: BTreeSet<String>,
    strict_routes: bool,
    image_input: HashMap<String, bool>,
    fallbacks: HashMap<String, Vec<String>>,
    selected: Mutex<HashMap<(String, String), String>>,
    auxiliary_selected: Mutex<HashMap<(String, String), String>>,
    usage_store: Mutex<Option<Store>>,
    quota_host: Mutex<Option<Arc<crate::quota::QuotaHost>>>,
    accounts: HashMap<String, String>,
    context_route: crate::context_routes::ContextRoute,
    retry: RetryPolicy,
}
/// Usage and quota readings from failed attempts that the policy will retry.
#[derive(Default)]
struct Attempts {
    usage: Option<Usage>,
    quota: Vec<QuotaReading>,
}
impl ProviderRegistry {
    pub fn new(default_id: impl Into<String>, provider: Arc<dyn Provider>) -> Self {
        let default = default_id.into();
        Self {
            providers: vec![(default.clone(), provider)],
            decisions: Vec::new(),
            default,
            last: Mutex::default(),
            resets: CursorResets::new(),
            origins: HashMap::new(),
            configured_models: HashMap::new(),
            removed_models: BTreeSet::new(),
            strict_routes: false,
            image_input: HashMap::new(),
            fallbacks: HashMap::new(),
            selected: Mutex::default(),
            auxiliary_selected: Mutex::default(),
            usage_store: Mutex::default(),
            quota_host: Mutex::default(),
            accounts: HashMap::new(),
            context_route: Default::default(),
            retry: RetryPolicy::default(),
        }
    }
    /// One policy for every call this registry makes: main turns, decisions,
    /// auxiliary role calls and summary forks.
    pub fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry = policy;
    }
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }
    /// Live configured hosts reject missing explicit route prefixes. Plain library
    /// registries retain their existing vendor/model default-route compatibility.
    pub fn with_strict_routes(mut self) -> Self {
        self.strict_routes = true;
        self
    }
    pub fn insert(&mut self, id: impl Into<String>, provider: Arc<dyn Provider>) -> Result<()> {
        let id = id.into();
        ensure!(
            !id.is_empty() && !id.contains('/'),
            "Provider id must be non-empty and contain no '/': {id:?}"
        );
        ensure!(self.get(&id).is_none(), "Duplicate provider id: {id}");
        self.providers.push((id, provider));
        Ok(())
    }
    pub fn default_id(&self) -> &str {
        &self.default
    }
    /// Assign a stable account to a configured provider or decision route.
    pub fn set_account(&mut self, route: impl Into<String>, account: impl Into<String>) {
        self.accounts.insert(route.into(), account.into());
    }
    /// Account for a route name, qualified model or `llm/` decision model.
    /// Unqualified models use the default provider. Unmapped routes use their name.
    pub fn account(&self, model: &str) -> String {
        let model = model.strip_prefix("llm/").unwrap_or(model);
        let route = model
            .split_once('/')
            .map(|(route, _)| route)
            .unwrap_or(model);
        let route = if self.accounts.contains_key(route)
            || self.get(route).is_some()
            || self.get_decision(route).is_some()
        {
            route
        } else {
            &self.default
        };
        self.accounts
            .get(route)
            .map_or(route, String::as_str)
            .to_owned()
    }
    /// Hosts bind the authoritative chat store independently of provider generations.
    pub fn set_usage_store(&self, store: Store) {
        *self.usage_store.lock().unwrap() = Some(self.attach_context_log(store));
    }
    pub fn set_quota_host(&self, host: Arc<crate::quota::QuotaHost>) {
        *self.quota_host.lock().unwrap() = Some(host);
    }
    async fn quota_request<F, Fut>(
        &self,
        provider: &str,
        account: &str,
        model: &str,
        cancellation: CancellationToken,
        mut call: F,
    ) -> Result<ModelResponse>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<ModelResponse>>,
    {
        let host = self.quota_host.lock().unwrap().clone();
        match host {
            Some(host) => {
                host.execute(provider, account, model, cancellation, call)
                    .await
            }
            None => call().await,
        }
    }
    /// Only explicitly configured alternatives may be attempted. New registry
    /// generations start without sticky selections after a config reload.
    pub fn set_model_fallbacks(
        &mut self,
        primary: String,
        alternatives: Vec<String>,
    ) -> Result<()> {
        let mut unique = BTreeSet::new();
        for model in std::iter::once(&primary).chain(alternatives.iter()) {
            ensure!(
                model
                    .split_once('/')
                    .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
                    && unique.insert(model.clone()),
                "Fallback models must be distinct qualified routes"
            );
        }
        self.fallbacks.insert(primary, alternatives);
        Ok(())
    }
    pub fn selected_model(response: &ModelResponse) -> Option<&str> {
        response
            .provider_state
            .as_ref()?
            .get("_bubblepaw_selected_model")?
            .as_str()
    }
    /// Exact configured model capability; no model-name inference or fallback.
    pub fn set_image_input(&mut self, provider: &str, model: &str, enabled: bool) {
        self.image_input
            .insert(format!("{provider}/{model}"), enabled);
    }
    pub fn insert_decision(
        &mut self,
        id: impl Into<String>,
        provider: Arc<dyn DecisionProvider>,
    ) -> Result<()> {
        let id = id.into();
        ensure!(
            !id.is_empty() && !id.contains('/') && id != "llm",
            "Invalid decision provider id: {id:?}"
        );
        ensure!(
            self.get_decision(&id).is_none(),
            "Duplicate decision provider id: {id}"
        );
        self.decisions.push((id, provider));
        Ok(())
    }
    pub fn get_decision(&self, id: &str) -> Option<Arc<dyn DecisionProvider>> {
        self.decisions
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, provider)| provider.clone())
    }
    pub fn decision_available(&self, model: &str) -> bool {
        match model.split_once('/') {
            Some(("llm", model)) => self.llm_available(model),
            Some((id, _)) => {
                self.get_decision(id).is_some() && !self.removed_models.contains(model)
            }
            None => false,
        }
    }
    /// Model name forwarded to a selected decision adapter.
    pub fn decision_model_name(&self, model: &str) -> String {
        match model.split_once('/') {
            Some(("llm", model)) => self.route(model).2,
            Some((_, model)) => model.into(),
            None => model.into(),
        }
    }
    pub fn decision_uses_llm(&self, model: &str) -> bool {
        model.starts_with("llm/")
    }
    /// Wire shape of the chat request an `llm/` decision candidate sends, for
    /// prompt-cache accounting. Dedicated decision adapters have no shape.
    pub fn decision_wire_shape(
        &self,
        candidate: &str,
        request: &DecisionRequest,
    ) -> Option<bubblepaw_core::WireShape> {
        let model = candidate.strip_prefix("llm/")?;
        let mut request = request.clone();
        request.model = model.to_owned();
        self.wire_shape(&super::LlmDecisions::model_request(&request).ok()?)
    }
    /// Auxiliary role work retries the actual candidate route, rather than
    /// restarting an unavailable primary and its entire fallback chain.
    pub async fn generate_auxiliary(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
        decision: bool,
        retry_base_delay: Duration,
    ) -> Result<ModelResponse> {
        let session = request
            .messages
            .iter()
            .rev()
            .find(|message| !message.session_id.is_empty())
            .map(|message| message.session_id.clone())
            .unwrap_or_else(|| request.session_id.clone());
        self.auxiliary_usage(
            &session,
            RequestKind::SummaryFork,
            self.generate_routed(
                request,
                Arc::new(|_| {}),
                cancellation,
                decision,
                Some(retry_base_delay),
            ),
        )
        .await
    }
    pub fn llm_available(&self, model: &str) -> bool {
        model
            .split_once('/')
            .is_none_or(|(id, _)| self.get(id).is_some())
            && self.checked_route(model).is_ok()
    }
    pub async fn decide_with_reasoning(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<(DecisionResponse, Option<String>)> {
        let session = request.session_id.clone().unwrap_or_default();
        self.auxiliary_usage(
            &session,
            RequestKind::Decider,
            // Native and LLM routes share a large future. Keep it off nested
            // role/accounting wrapper stacks, including the default test stack.
            Box::pin(self.decide_with_reasoning_inner(request, cancellation)),
        )
        .await
    }
    async fn decide_with_reasoning_inner(
        &self,
        mut request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<(DecisionResponse, Option<String>)> {
        let (id, model) = request
            .model
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("Decision model needs an adapter prefix"))?;
        let id = id.to_owned();
        let model = model.to_owned();
        if id == "llm" {
            ensure!(
                self.llm_available(&model),
                "Missing LLM provider for {model}"
            );
            let reported_model = self.checked_route(&model)?.2;
            request.model = model;
            return super::LlmDecisions::decide_using(
                self,
                request,
                cancellation,
                Some(&reported_model),
                &self.context_route,
            )
            .await;
        }
        if self.removed_models.contains(&format!("{id}/{model}")) {
            return Err(ProviderError::new(
                ProviderErrorKind::Fatal,
                format!("Model {id}/{model} was removed from provider configuration"),
            )
            .into());
        }
        request.model = model.clone();
        let provider = self.get_decision(&id).ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Fatal,
                format!("Missing decision provider: {id}"),
            )
        })?;
        let account = self.account(&id);
        let published = AtomicBool::new(false);
        // Usage of the attempts the policy repeated; the final result carries it.
        let repeated = Mutex::new(DecisionUsage::default());
        let response = bubblepaw_core::with_request_usage_account(&account, async {
            let response = with_retry(
                &id,
                &self.retry,
                &cancellation,
                &published,
                || {
                    let request = request.clone();
                    let cancellation = cancellation.clone();
                    let provider = provider.clone();
                    let id = id.clone();
                    let model = model.clone();
                    async move {
                        let mut record = RequestUsage::new(&id, &model);
                        let before = bubblepaw_core::request_usage_count();
                        let response = bubblepaw_core::with_request_usage_provider(
                            &id,
                            provider.decide(request, cancellation),
                        )
                        .await;
                        if bubblepaw_core::request_usage_count() == before {
                            record.failed = response.is_err();
                            record.usage = match &response {
                                Ok(response) => response.usage.clone().into(),
                                Err(error) => ProviderError::of(error)
                                    .and_then(|error| error.usage.clone())
                                    .or_else(|| failed_decision_usage(error).map(Into::into))
                                    .unwrap_or_default(),
                            };
                            bubblepaw_core::record_request_usage(record);
                        }
                        response
                    }
                },
                |error| {
                    if let Some(usage) = failed_decision_usage(error) {
                        repeated.lock().unwrap().add(&usage);
                    }
                },
            )
            .await;
            let repeated = repeated.lock().unwrap().clone();
            match response {
                Ok(mut response) => {
                    response.usage.add(&repeated);
                    Ok(response)
                }
                Err(error) if repeated == DecisionUsage::default() => Err(error),
                Err(error) => {
                    let mut total = repeated;
                    if let Some(usage) = failed_decision_usage(&error) {
                        total.add(&usage);
                    }
                    Err(super::with_decision_usage(error, total))
                }
            }
        })
        .await;
        Ok((response?, None))
    }
    pub fn ids(&self) -> Vec<String> {
        self.providers.iter().map(|(id, _)| id.clone()).collect()
    }
    pub fn get(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, provider)| provider.clone())
    }
    /// Bind replay provenance to an exact configured route and endpoint.
    pub fn configure_route(&mut self, id: &str, endpoint: String, models: BTreeSet<String>) {
        self.origins.insert(id.to_owned(), endpoint);
        self.configured_models.insert(id.to_owned(), models);
    }
    pub fn set_removed_models(&mut self, models: BTreeSet<String>) {
        self.removed_models = models;
    }
    /// Configured live hosts never remap missing explicit routes to the default.
    pub fn checked_route(&self, model: &str) -> Result<(String, Arc<dyn Provider>, String)> {
        let (id, forwarded) = model.split_once('/').unwrap_or((&self.default, model));
        let fail = |message: String| {
            anyhow::Error::new(ProviderError::new(ProviderErrorKind::Fatal, message))
        };
        let (id, provider, forwarded) = match self.get(id) {
            Some(provider) => (id.to_owned(), provider, forwarded.to_owned()),
            None if !self.strict_routes => self.route(model),
            None => {
                return Err(fail(format!(
                    "Provider route {id} is unavailable or was removed"
                )));
            }
        };
        if forwarded.is_empty() {
            return Err(fail(format!("Model for provider route {id} is empty")));
        }
        if self.removed_models.contains(&format!("{id}/{forwarded}")) {
            return Err(fail(format!(
                "Model {id}/{forwarded} was removed from provider configuration"
            )));
        }
        Ok((id, provider, forwarded))
    }
    /// The primary route for `request` and the route its session is sticky to
    /// (the primary itself when none is selected yet), both as `provider/model`.
    fn primary_and_selected(&self, request: &ModelRequest) -> (String, String) {
        let primary = self
            .checked_route(&request.model)
            .map(|(id, _, model)| format!("{id}/{model}"))
            .unwrap_or_else(|_| request.model.clone());
        let session = if request.session_id.is_empty() {
            request
                .messages
                .iter()
                .rev()
                .find(|message| !message.session_id.is_empty())
                .map(|message| message.session_id.as_str())
                .unwrap_or_default()
        } else {
            &request.session_id
        };
        let selected = self
            .selected
            .lock()
            .unwrap()
            .get(&(session.to_owned(), primary.clone()))
            .cloned()
            .unwrap_or(primary.clone());
        (primary, selected)
    }
    fn prepare_replay(&self, id: &str, request: &mut ModelRequest) {
        let Some(endpoint) = self.origins.get(id) else {
            return;
        };
        let expected = serde_json::json!({"provider":id,"endpoint":endpoint});
        for message in &mut request.messages {
            let Some(state) = message.provider_state.as_mut() else {
                continue;
            };
            // Pre-tag state inherits this route; adapter/model compatibility is
            // still checked by the destination adapter's native replay guard.
            if state
                .get("_bubblepaw_route_origin")
                .is_none_or(|origin| origin == &expected)
            {
                continue;
            }
            // Preserve opaque state and its provenance; only the transient
            // serving copy is made incompatible with the destination adapter.
            // Messages then rejects an unfinished foreign signed tool loop.
            if let Some(envelope) = state.as_object_mut() {
                let origin = envelope
                    .entry(bubblepaw_core::PROVIDER_STATE_ORIGIN_FIELD)
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(origin) = origin.as_object_mut() {
                    origin.insert(
                        "provider_kind".into(),
                        serde_json::json!("incompatible-provider-route"),
                    );
                } else {
                    *origin = serde_json::json!({"provider_kind":"incompatible-provider-route"});
                }
            }
        }
    }
    fn record_origin(&self, id: &str, response: &mut ModelResponse) {
        if let Some(endpoint) = self.origins.get(id)
            && let Some(state) = response
                .provider_state
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
        {
            state.insert(
                "_bubblepaw_route_origin".into(),
                serde_json::json!({"provider":id,"endpoint":endpoint}),
            );
        }
    }
    /// Provider id, provider, and the model name to forward.
    pub fn route(&self, model: &str) -> (String, Arc<dyn Provider>, String) {
        if let Some((prefix, rest)) = model.split_once('/')
            && let Some(provider) = self.get(prefix)
        {
            return (prefix.into(), provider, rest.into());
        }
        (
            self.default.clone(),
            self.get(&self.default).expect("default provider"),
            model.into(),
        )
    }
    async fn generate_routed(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
        decision: bool,
        retry_base_delay: Option<Duration>,
    ) -> Result<ModelResponse> {
        let primary = self
            .checked_route(&request.model)
            .map(|(id, _, model)| format!("{id}/{model}"))
            .unwrap_or_else(|_| request.model.clone());
        let logical_session = request.session_id.clone();
        // Stateless auxiliary work can reuse the confirmed main route, but its
        // own fallbacks must never switch that route or a main-session mirror.
        // Its source message still supplies the wire header.
        let source_session = if logical_session.is_empty() {
            request
                .messages
                .iter()
                .rev()
                .find(|message| !message.session_id.is_empty())
                .map(|message| message.session_id.clone())
                .unwrap_or_default()
        } else {
            logical_session.clone()
        };
        let key = (source_session.clone(), primary.clone());
        let selections = if logical_session.is_empty() {
            &self.auxiliary_selected
        } else {
            &self.selected
        };
        let mut candidates = vec![primary.clone()];
        let alternatives = self.fallbacks.get(&primary).cloned().unwrap_or_default();
        let has_fallbacks = !alternatives.is_empty();
        candidates.extend(alternatives);
        let selected = selections.lock().unwrap().get(&key).cloned().or_else(|| {
            logical_session
                .is_empty()
                .then(|| self.selected.lock().unwrap().get(&key).cloned())
                .flatten()
        });
        if !source_session.is_empty()
            && let Some(selected) = selected
            && let Some(index) = candidates
                .iter()
                .position(|candidate| candidate == &selected)
        {
            candidates.drain(..index);
        }
        let mut failed = Vec::new();
        // Classified message per failed route (e.g. a subscription usage limit
        // with its type and reset time); never raw provider text.
        let mut reasons = Vec::new();
        let mut accumulated: Option<Usage> = None;
        let mut last_error = None;
        // Auxiliary role work keeps its own base delay; everything else uses the
        // registry policy.
        let policy = match retry_base_delay {
            Some(base) => RetryPolicy {
                initial_delay: base,
                ..self.retry.clone()
            },
            None => self.retry.clone(),
        };
        for candidate in candidates {
            if cancellation.is_cancelled() {
                return Err(with_usage(super::cancelled(), accumulated.as_ref(), false));
            }
            let (id, provider, model) = match self.checked_route(&candidate) {
                Ok(route) => route,
                Err(error) => {
                    failed.push(candidate);
                    reasons.push("route unavailable".to_owned());
                    last_error = Some(error);
                    continue;
                }
            };
            let mut next = request.clone();
            next.model = model.clone();
            self.prepare_replay(&id, &mut next);
            if !logical_session.is_empty() {
                let previous = self
                    .last
                    .lock()
                    .unwrap()
                    .insert(logical_session.clone(), id.clone());
                // Cursor mirrors belong to the actual provider. A model change
                // on that provider preserves history; fallback selection has
                // its own source-session/model key above.
                if let Some(previous) = previous.filter(|previous| previous != &id) {
                    if let Some(old) = self.get(&previous) {
                        let _ = old.close_session(&logical_session).await;
                    }
                    self.resets.reset(&logical_session, &mut next.history);
                } else {
                    self.resets.apply(&logical_session, &mut next.history);
                }
            }
            let published = Arc::new(AtomicBool::new(false));
            let visible = published.clone();
            let callback = on_text.clone();
            let guarded: TextCallback = Arc::new(move |text| {
                if !text.is_empty() {
                    visible.store(true, Ordering::Release);
                }
                callback(text);
            });
            let account = self.account(&id);
            let attempt_state = Arc::new(Mutex::new(Attempts::default()));
            // One nesting order for main turns and forks: the quota host drives
            // the retry loop, so its policy scope covers every attempt and
            // rate-limit attempts are not multiplied by the transport policy.
            let call = || {
                let request = next.clone();
                let on_text = guarded.clone();
                let cancellation = cancellation.clone();
                let published = published.clone();
                let id = id.clone();
                let model = model.clone();
                let provider = provider.clone();
                async move {
                    let record = RequestUsage::new(&id, &model);
                    let before = bubblepaw_core::request_usage_count();
                    let result = if decision {
                        provider
                            .generate_decision(request, on_text, cancellation)
                            .await
                    } else {
                        provider.generate(request, on_text, cancellation).await
                    };
                    let result = result.map_err(|error| {
                        super::mark_published(error, published.load(Ordering::Acquire))
                    });
                    if bubblepaw_core::request_usage_count() == before {
                        record_model_result(record, &result);
                    }
                    if result.is_err()
                        && let Some(start) = before
                    {
                        bubblepaw_core::discard_request_usage_since(start);
                    }
                    result
                }
            };
            let on_failed_attempt = |error: &anyhow::Error| {
                let mut attempt_state = attempt_state.lock().unwrap();
                if let Some(usage) = ProviderError::of(error).and_then(|error| error.usage.as_ref())
                {
                    add_usage(&mut attempt_state.usage, usage);
                }
                if let Some(typed) = ProviderError::of(error) {
                    attempt_state.quota.extend(typed.quota.iter().cloned());
                }
            };
            let result = bubblepaw_core::with_request_usage_account(
                &account,
                bubblepaw_core::with_request_usage_provider(
                    &id,
                    self.quota_request(&id, &account, &model, cancellation.clone(), || {
                        super::with_retry(
                            &id,
                            &policy,
                            &cancellation,
                            &published,
                            &call,
                            &on_failed_attempt,
                        )
                    }),
                ),
            )
            .await;
            let attempt_state = std::mem::take(&mut *attempt_state.lock().unwrap());
            if let Some(usage) = &attempt_state.usage {
                add_usage(&mut accumulated, usage);
            }
            let result = match result {
                Ok(mut response) => {
                    if !attempt_state.quota.is_empty() {
                        let mut quota = attempt_state.quota;
                        quota.extend(response.quota);
                        response.quota = quota;
                    }
                    Ok(response)
                }
                Err(error) => Err(super::retain_prior_quota(error, attempt_state.quota)),
            };
            match result {
                Ok(mut response) => {
                    if accumulated.is_some() && response.last_request_usage.is_none() {
                        response.last_request_usage = Some(LastRequestUsage {
                            input_tokens: response.usage.as_ref().map(|usage| usage.input_tokens),
                            output_tokens: response.usage.as_ref().map(|usage| usage.output_tokens),
                        });
                    }
                    if let Some(usage) = &response.usage {
                        add_usage(&mut accumulated, usage);
                    }
                    response.usage = accumulated;
                    self.record_origin(&id, &mut response);
                    if has_fallbacks {
                        let state = response
                            .provider_state
                            .get_or_insert_with(|| serde_json::json!({}));
                        if let Some(state) = state.as_object_mut() {
                            state.insert(
                                "_bubblepaw_selected_model".into(),
                                serde_json::json!(candidate),
                            );
                            if !failed.is_empty() {
                                state.insert(
                                    "_bubblepaw_fallback_from".into(),
                                    serde_json::json!(failed),
                                );
                                state.insert(
                                    "_bubblepaw_fallback_reasons".into(),
                                    serde_json::json!(reasons),
                                );
                            }
                        }
                        // A fallback route also has provenance when its adapter
                        // returned no native replay state.
                        self.record_origin(&id, &mut response);
                        if !source_session.is_empty() {
                            selections.lock().unwrap().insert(key, candidate);
                        }
                    }
                    return Ok(response);
                }
                Err(error) => {
                    if let Some(usage) =
                        ProviderError::of(&error).and_then(|error| error.usage.as_ref())
                    {
                        add_usage(&mut accumulated, usage);
                    }
                    let published = published.load(Ordering::Acquire)
                        || ProviderError::of(&error).is_some_and(|error| error.published);
                    let stopped = cancellation.is_cancelled()
                        || ProviderError::of(&error)
                            .is_some_and(|error| error.message == "cancelled");
                    if published
                        || stopped
                        || crate::quota::is_quota_stop(&error)
                        || ProviderError::of(&error).is_some_and(super::is_terminal_quota_error)
                    {
                        return Err(with_usage(error, accumulated.as_ref(), published));
                    }
                    let reason = fallback_reason(&error);
                    tracing::warn!(target: "bubblepaw::provider", route = %candidate, reason = %reason, "model route failed");
                    failed.push(candidate);
                    reasons.push(reason);
                    last_error = Some(error);
                }
            }
        }
        Err(with_usage(
            last_error.unwrap_or_else(|| anyhow::anyhow!("No configured model route is available")),
            accumulated.as_ref(),
            false,
        ))
    }
}
/// Why a route was skipped, from the classified kind only. An extension usage
/// limit keeps its sanitized limit type and reset time (extension.rs rpc_error).
fn fallback_reason(error: &anyhow::Error) -> String {
    let Some(error) = ProviderError::of(error) else {
        return "provider error".into();
    };
    match &error.kind {
        ProviderErrorKind::RateLimit { .. }
            if error.message.starts_with("extension usage limit reached") =>
        {
            error.message.clone()
        }
        ProviderErrorKind::RateLimit {
            resets_at: Some(resets_at),
        } => format!("rate_limit, resets {resets_at}"),
        ProviderErrorKind::RateLimit { resets_at: None } => "rate_limit".into(),
        ProviderErrorKind::Auth => "auth".into(),
        ProviderErrorKind::Context => "context".into(),
        ProviderErrorKind::Transient => "transient".into(),
        ProviderErrorKind::Fatal => "fatal".into(),
    }
}
/// Usage a failed decision attempt reports, from its decision metadata or its provider error.
fn failed_decision_usage(error: &anyhow::Error) -> Option<DecisionUsage> {
    super::decision_error_usage(error).cloned().or_else(|| {
        ProviderError::of(error)
            .and_then(|error| error.usage.as_ref())
            .map(DecisionUsage::from)
    })
}
fn with_usage(error: anyhow::Error, usage: Option<&Usage>, published: bool) -> anyhow::Error {
    if usage.is_none() && !published {
        return error;
    }
    let mut typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    typed.published |= published;
    typed.usage = usage.cloned();
    error.context(typed)
}
#[async_trait]
impl DecisionProvider for ProviderRegistry {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        self.decide_with_reasoning(request, cancellation)
            .await
            .map(|(response, _)| response)
    }
}
#[async_trait]
impl Provider for ProviderRegistry {
    fn summary_fork_routes(&self, model: &str) -> Vec<String> {
        let primary = self
            .checked_route(model)
            .map(|(id, _, model)| format!("{id}/{model}"))
            .unwrap_or_else(|_| model.to_owned());
        let mut routes = vec![primary.clone()];
        routes.extend(self.fallbacks.get(&primary).into_iter().flatten().cloned());
        routes
    }
    fn estimate_summary_fork_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&serde_json::Value) -> usize,
    ) -> Result<Option<usize>> {
        let (id, provider, model) = self.checked_route(&request.model)?;
        let mut routed = request.clone();
        routed.model = model;
        self.prepare_replay(&id, &mut routed);
        provider.estimate_summary_fork_tokens(&routed, count)
    }
    fn supports_summary_fork(&self, model: &str) -> bool {
        self.checked_route(model)
            .is_ok_and(|(_, provider, model)| provider.supports_summary_fork(&model))
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<serde_json::Value>> {
        let (id, provider, model) = self.checked_route(&request.model)?;
        let mut routed = request.clone();
        routed.model = model;
        self.prepare_replay(&id, &mut routed);
        provider.summary_fork_mapping(&routed)
    }
    async fn generate_summary_fork(
        &self,
        mut request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let session = request
            .messages
            .iter()
            .rev()
            .find(|message| !message.session_id.is_empty())
            .map(|message| message.session_id.clone())
            .unwrap_or_else(|| request.session_id.clone());
        self.auxiliary_usage(&session, RequestKind::SummaryFork, async {
            // The capture identifies the actual successful main route. Deliberately
            // bypass fallback/sticky-route bookkeeping and all main-session mirrors.
            let (id, provider, model) = self.checked_route(&request.model)?;
            ensure!(
                provider.supports_summary_fork(&model),
                "Unsupported summary fork route"
            );
            request.model = model;
            request.session_id.clear();
            request.history = Default::default();
            self.prepare_replay(&id, &mut request);
            let account = self.account(&id);
            // Forks have no listener, so no attempt can be published.
            let published = AtomicBool::new(false);
            // Usage and quota of the attempts the policy repeated.
            let repeated = Mutex::new(Attempts::default());
            let response = bubblepaw_core::with_request_usage_account(&account, async {
                bubblepaw_core::with_request_usage_provider(
                    &id,
                    self.quota_request(&id, &account, &request.model, cancellation.clone(), || {
                        with_retry(
                            &id,
                            &self.retry,
                            &cancellation,
                            &published,
                            || {
                                let id = id.clone();
                                let model = request.model.clone();
                                let request = request.clone();
                                let cancellation = cancellation.clone();
                                let provider = provider.clone();
                                async move {
                                    let record = RequestUsage::new(&id, &model);
                                    let before = bubblepaw_core::request_usage_count();
                                    let response = bubblepaw_core::with_request_usage_provider(
                                        &id,
                                        provider.generate_summary_fork(request, cancellation),
                                    )
                                    .await;
                                    if bubblepaw_core::request_usage_count() == before {
                                        record_model_result(record, &response);
                                    }
                                    if response.is_err()
                                        && let Some(start) = before
                                    {
                                        bubblepaw_core::discard_request_usage_since(start);
                                    }
                                    response
                                }
                            },
                            |error| {
                                let mut state = repeated.lock().unwrap();
                                if let Some(usage) =
                                    ProviderError::of(error).and_then(|error| error.usage.as_ref())
                                {
                                    add_usage(&mut state.usage, usage);
                                }
                                if let Some(typed) = ProviderError::of(error) {
                                    state.quota.extend(typed.quota.iter().cloned());
                                }
                            },
                        )
                    }),
                )
                .await
            })
            .await;
            let Attempts {
                usage: repeated_usage,
                quota: repeated_quota,
            } = repeated.into_inner().unwrap();
            let response = match response {
                Ok(mut response) => {
                    if let Some(usage) = &repeated_usage {
                        add_usage(&mut response.usage, usage);
                    }
                    if !repeated_quota.is_empty() {
                        let mut quota = repeated_quota;
                        quota.extend(response.quota);
                        response.quota = quota;
                    }
                    Ok(response)
                }
                Err(error) => {
                    let mut total = repeated_usage;
                    if let Some(usage) =
                        ProviderError::of(&error).and_then(|error| error.usage.as_ref())
                    {
                        add_usage(&mut total, usage);
                    }
                    let error = with_usage(error, total.as_ref(), false);
                    Err(super::retain_prior_quota(error, repeated_quota))
                }
            };
            let mut response = response?;
            self.record_origin(&id, &mut response);
            Ok(response)
        })
        .await
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&serde_json::Value) -> usize,
    ) -> Result<Option<usize>> {
        let (primary, selected) = self.primary_and_selected(request);
        let mut candidates = vec![primary.clone()];
        candidates.extend(self.fallbacks.get(&primary).into_iter().flatten().cloned());
        if let Some(index) = candidates
            .iter()
            .position(|candidate| candidate == &selected)
        {
            candidates.drain(..index);
        }
        let mut largest = None;
        for candidate in candidates {
            let Ok((id, provider, model)) = self.checked_route(&candidate) else {
                continue;
            };
            let mut routed = request.clone();
            routed.model = model;
            self.prepare_replay(&id, &mut routed);
            if let Ok(Some(tokens)) = provider.estimate_input_tokens(&routed, count) {
                largest = Some(largest.unwrap_or(0).max(tokens));
            }
        }
        Ok(largest)
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<bubblepaw_core::WireShape> {
        // Only the route the main loop tries first: the session's sticky route,
        // else the primary. Fallbacks are not shaped.
        let (_, selected) = self.primary_and_selected(request);
        let (id, provider, model) = self.checked_route(&selected).ok()?;
        let mut routed = request.clone();
        routed.model = model;
        self.prepare_replay(&id, &mut routed);
        let mut shape = provider.wire_shape(&routed)?;
        shape.provider = id;
        shape.model = routed.model;
        Some(shape)
    }
    async fn generate_decision(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let session = request.session_id.clone();
        self.auxiliary_usage(
            &session,
            RequestKind::Decider,
            self.generate_routed(request, on_text, cancellation, true, None),
        )
        .await
    }
    fn models(&self) -> Vec<ModelInfo> {
        self.providers
            .iter()
            .flat_map(|(id, provider)| {
                let mut models = provider.models();
                if let Some(configured) = self.configured_models.get(id) {
                    for model in configured {
                        if !models.iter().any(|known| &known.id == model) {
                            models.push(ModelInfo {
                                id: model.clone(),
                                name: model.clone(),
                            });
                        }
                    }
                }
                models.into_iter().map(move |model| ModelInfo {
                    id: format!("{id}/{}", model.id),
                    name: model.name,
                })
            })
            .collect()
    }
    fn capabilities(&self, model: &str) -> Capabilities {
        self.checked_route(model).map_or_else(
            |_| Capabilities::default(),
            |(id, provider, model)| {
                let mut capabilities = provider.capabilities(&model);
                if let Some(enabled) = self.image_input.get(&format!("{id}/{model}")) {
                    capabilities.image_input = *enabled;
                }
                capabilities
            },
        )
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let session = request.session_id.clone();
        self.auxiliary_usage(
            &session,
            RequestKind::Main,
            self.generate_routed(request, on_text, cancellation, false, None),
        )
        .await
    }
    async fn close_session(&self, session_id: &str) -> Result<()> {
        self.last.lock().unwrap().remove(session_id);
        self.resets.forget(session_id);
        let mut first_error = None;
        for (_, provider) in &self.providers {
            if let Err(error) = provider.close_session(session_id).await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod auxiliary_retry_tests;
