//! Qualified `provider/model` routes share one harness provider; unqualified models use the default.
//! Explicit missing prefixes fail at dispatch instead of falling back.
use super::{RetryPolicy, add_usage, metering::record_model_result, with_retry};
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use bubblepaw_ai::{FailedAttempts, Router};
use bubblepaw_core::{
    Branch, BranchView, CancellationToken, Capabilities, DecisionProvider, DecisionRequest,
    DecisionResponse, DecisionUsage, ModelInfo, ModelRequest, ModelResponse, Provider,
    ProviderError, ProviderErrorKind, RequestKind, RequestUsage, Store, StreamCallback,
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

#[cfg(feature = "bench")]
#[path = "registry_bench.rs"]
mod benchmark;
#[path = "registry_context.rs"]
mod context_binding;
#[path = "registry_hooks.rs"]
mod hooks;
pub struct ProviderRegistry {
    router: Router,
    decisions: Vec<(String, Arc<dyn DecisionProvider>)>,
    usage_store: Mutex<Option<Store>>,
    quota_host: Mutex<Option<Arc<crate::quota::QuotaHost>>>,
    accounts: HashMap<String, String>,
    context_route: crate::context_routes::ContextRoute,
}
impl ProviderRegistry {
    pub fn new(default_id: impl Into<String>, provider: Arc<dyn Provider>) -> Self {
        Self {
            router: Router::new(default_id, provider),
            decisions: Vec::new(),
            usage_store: Mutex::default(),
            quota_host: Mutex::default(),
            accounts: HashMap::new(),
            context_route: Default::default(),
        }
    }
    /// One policy for every call this registry makes: main turns, decisions,
    /// auxiliary role calls and summary forks.
    pub fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.router.set_retry_policy(policy);
    }
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.router.set_retry_policy(policy);
        self
    }
    /// Live configured hosts reject missing explicit route prefixes. Plain library
    /// registries retain their existing vendor/model default-route compatibility.
    pub fn with_strict_routes(mut self) -> Self {
        self.router.set_strict_routes();
        self
    }
    pub fn insert(&mut self, id: impl Into<String>, provider: Arc<dyn Provider>) -> Result<()> {
        self.router.insert(id, provider)
    }
    pub fn default_id(&self) -> &str {
        self.router.default_id()
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
            self.router.default_id()
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
    /// A per-call copy of `policy` that yields rate limits exactly when a quota
    /// host will wrap the call.
    fn hosted(&self, policy: &RetryPolicy) -> RetryPolicy {
        RetryPolicy {
            yield_rate_limits: self.quota_host.lock().unwrap().is_some(),
            ..policy.clone()
        }
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
        self.router.set_model_fallbacks(primary, alternatives)
    }
    pub fn selected_model(response: &ModelResponse) -> Option<&str> {
        response.selected_route()
    }
    /// Exact configured model capability; no model-name inference or fallback.
    pub fn set_image_input(&mut self, provider: &str, model: &str, enabled: bool) {
        self.router.set_image_input(provider, model, enabled);
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
            Some((id, _)) => self.get_decision(id).is_some() && !self.router.is_removed(model),
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
        if self.router.is_removed(&format!("{id}/{model}")) {
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
                self.router.retry_policy(),
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
        self.router.ids()
    }
    pub fn get(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.router.get(id)
    }
    /// Bind replay provenance to an exact configured route and endpoint.
    pub fn configure_route(&mut self, id: &str, endpoint: String, models: BTreeSet<String>) {
        self.router.configure_route(id, endpoint, models);
    }
    pub fn set_removed_models(&mut self, models: BTreeSet<String>) {
        self.router.set_removed_models(models);
    }
    /// Configured live hosts never remap missing explicit routes to the default.
    pub fn checked_route(&self, model: &str) -> Result<(String, Arc<dyn Provider>, String)> {
        self.router.checked_route(model)
    }
    /// Provider id, provider, and the model name to forward.
    pub fn route(&self, model: &str) -> (String, Arc<dyn Provider>, String) {
        self.router.route(model)
    }
    async fn generate_routed(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
        cancellation: CancellationToken,
        decision: bool,
        retry_base_delay: Option<Duration>,
    ) -> Result<ModelResponse> {
        let hooks = hooks::Hooks::new(self);
        self.router
            .generate_routed(
                request,
                on_text,
                cancellation,
                decision,
                retry_base_delay,
                &hooks,
            )
            .await
    }
}
/// Usage a failed decision attempt reports, from its decision metadata or its provider error.
fn failed_decision_usage(error: &anyhow::Error) -> Option<DecisionUsage> {
    super::decision_error_usage(error).or_else(|| {
        ProviderError::of(error)
            .and_then(|error| error.usage.as_ref())
            .map(DecisionUsage::from)
    })
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
impl Branch for ProviderRegistry {
    fn routes(&self, model: &str) -> Vec<String> {
        self.router.routes(model)
    }
    fn view(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&serde_json::Value) -> usize,
    ) -> BranchView {
        let routed = || {
            let (id, provider, model) = self.checked_route(&request.model)?;
            let mut routed = request.clone();
            routed.model = model;
            self.router.prepare_replay(&id, &mut routed);
            Ok::<_, anyhow::Error>((provider, routed))
        };
        match routed() {
            Ok((provider, routed)) => BranchView::of(provider.as_ref(), &routed, count),
            Err(error) => BranchView {
                mapping: Err(error),
                estimate: Err(routed()
                    .err()
                    .unwrap_or_else(|| anyhow::anyhow!("route changed"))),
            },
        }
    }
    async fn generate_branch(
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
                provider.branch(&model).is_some(),
                "Unsupported summary fork route"
            );
            request.model = model;
            request.session_id.clear();
            request.history = Default::default();
            self.router.prepare_replay(&id, &mut request);
            let account = self.account(&id);
            // Forks have no listener, so no attempt can be published.
            let published = AtomicBool::new(false);
            // Usage and quota of the attempts the policy repeated.
            let repeated = Mutex::new(FailedAttempts::default());
            let hosted_policy = self.hosted(self.router.retry_policy());
            let response = bubblepaw_core::with_request_usage_account(&account, async {
                bubblepaw_core::with_request_usage_provider(
                    &id,
                    self.quota_request(&id, &account, &request.model, cancellation.clone(), || {
                        with_retry(
                            &id,
                            &hosted_policy,
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
                                    let response =
                                        bubblepaw_core::with_request_usage_provider(&id, async {
                                            provider
                                                .branch(&model)
                                                .context("Unsupported summary fork route")?
                                                .generate_branch(request, cancellation)
                                                .await
                                        })
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
                            |error| repeated.lock().unwrap().record(error),
                        )
                    }),
                )
                .await
            })
            .await;
            let FailedAttempts {
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
                    let error = bubblepaw_ai::with_usage(error, total.as_ref(), false);
                    Err(super::retain_prior_quota(error, repeated_quota))
                }
            };
            let mut response = response?;
            self.router.record_origin(&id, &mut response);
            Ok(response)
        })
        .await
    }
}
#[async_trait]
impl Provider for ProviderRegistry {
    fn branch(&self, model: &str) -> Option<&dyn Branch> {
        self.checked_route(model)
            .is_ok_and(|(_, provider, model)| provider.branch(&model).is_some())
            .then_some(self as &dyn Branch)
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&serde_json::Value) -> usize,
    ) -> Result<Option<usize>> {
        self.router.estimate_input_tokens(request, count)
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<bubblepaw_core::WireShape> {
        self.router.wire_shape(request)
    }
    async fn generate_decision(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
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
        self.router.models()
    }
    fn capabilities(&self, model: &str) -> Capabilities {
        self.router.capabilities(model)
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
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
        self.router.close_session(session_id).await
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod auxiliary_retry_tests;
