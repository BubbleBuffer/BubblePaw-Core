//! Qualified `provider/model` routes share one provider map; unqualified models use
//! the default. Explicit missing prefixes fail at dispatch instead of falling back.
//!
//! The router owns the routing: which providers exist, the fallback order of a
//! route, the route a session is sticky to, replay preparation, and the usage
//! totals of failed attempts. Billing, quota, accounts and logging attach through a
//! [`RouteObserver`]. `router_generate.rs` holds the dispatch loop.
use crate::{
    Capabilities, CursorResets, ModelInfo, ModelRequest, ModelResponse,
    PROVIDER_STATE_ORIGIN_FIELD, Provider, ProviderError, ProviderErrorKind, WireShape,
    adapter::RetryPolicy,
};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
};

type Selections = Mutex<HashMap<(String, String), String>>;

pub struct Router {
    providers: Vec<(String, Arc<dyn Provider>)>,
    default: String,
    pub(crate) last: Mutex<HashMap<String, String>>,
    pub(crate) resets: CursorResets,
    origins: HashMap<String, String>,
    configured_models: HashMap<String, BTreeSet<String>>,
    removed_models: BTreeSet<String>,
    strict_routes: bool,
    image_input: HashMap<String, bool>,
    fallbacks: HashMap<String, Vec<String>>,
    pub(crate) selected: Selections,
    pub(crate) auxiliary_selected: Selections,
    retry: RetryPolicy,
}

impl Router {
    pub fn new(default_id: impl Into<String>, provider: Arc<dyn Provider>) -> Self {
        let default = default_id.into();
        Self {
            providers: vec![(default.clone(), provider)],
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
            retry: RetryPolicy::default(),
        }
    }
    /// One policy for every call this router makes.
    pub fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry = policy;
    }
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry
    }
    /// Missing explicit route prefixes fail instead of reaching the default.
    pub fn set_strict_routes(&mut self) {
        self.strict_routes = true;
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
    /// Provider entries in insertion order, default first.
    pub fn entries(&self) -> &[(String, Arc<dyn Provider>)] {
        &self.providers
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
    /// Only explicitly configured alternatives may be attempted. New routers
    /// start without sticky selections after a config reload.
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
    /// Exact configured model capability; no model-name inference or fallback.
    pub fn set_image_input(&mut self, provider: &str, model: &str, enabled: bool) {
        self.image_input
            .insert(format!("{provider}/{model}"), enabled);
    }
    /// Bind replay provenance to an exact configured route and endpoint.
    pub fn configure_route(&mut self, id: &str, endpoint: String, models: BTreeSet<String>) {
        self.origins.insert(id.to_owned(), endpoint);
        self.configured_models.insert(id.to_owned(), models);
    }
    pub fn set_removed_models(&mut self, models: BTreeSet<String>) {
        self.removed_models = models;
    }
    pub fn is_removed(&self, route: &str) -> bool {
        self.removed_models.contains(route)
    }
    /// Copy the settings of `other` that are not providers or live state: origins,
    /// configured and removed models, strictness, image input and the retry policy.
    pub fn copy_settings_from(&mut self, other: &Self) {
        self.origins = other.origins.clone();
        self.configured_models = other.configured_models.clone();
        self.removed_models = other.removed_models.clone();
        self.strict_routes = other.strict_routes;
        self.image_input = other.image_input.clone();
        self.retry = other.retry.clone();
    }
    /// Whether `other` carries the same origins, configured models and image input.
    #[doc(hidden)]
    pub fn same_settings(&self, other: &Self) -> bool {
        self.origins == other.origins
            && self.configured_models == other.configured_models
            && self.image_input == other.image_input
    }
    /// The route each session last used, by session id.
    #[doc(hidden)]
    pub fn last_routes(&self) -> &Mutex<HashMap<String, String>> {
        &self.last
    }
    /// Sticky selections of main sessions, by (session, primary route).
    #[doc(hidden)]
    pub fn selections(&self) -> &Selections {
        &self.selected
    }
    /// Sticky selections of stateless auxiliary calls.
    #[doc(hidden)]
    pub fn auxiliary_selections(&self) -> &Selections {
        &self.auxiliary_selected
    }
    #[doc(hidden)]
    pub fn fallbacks_mut(&mut self) -> &mut HashMap<String, Vec<String>> {
        &mut self.fallbacks
    }
    #[doc(hidden)]
    pub fn has_fallbacks(&self) -> bool {
        !self.fallbacks.is_empty()
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
    fn primary_of(&self, model: &str) -> String {
        self.checked_route(model)
            .map(|(id, _, model)| format!("{id}/{model}"))
            .unwrap_or_else(|_| model.to_owned())
    }
    /// The primary route followed by its configured fallbacks.
    pub fn routes(&self, model: &str) -> Vec<String> {
        self.chain(&self.primary_of(model))
    }
    /// `primary` followed by its configured fallbacks.
    pub(crate) fn chain(&self, primary: &str) -> Vec<String> {
        let mut routes = vec![primary.to_owned()];
        routes.extend(self.fallbacks.get(primary).into_iter().flatten().cloned());
        routes
    }
    /// The primary route for `request` and the route its session is sticky to
    /// (the primary itself when none is selected yet), both as `provider/model`.
    fn primary_and_selected(&self, request: &ModelRequest) -> (String, String) {
        let primary = self.primary_of(&request.model);
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
    pub fn prepare_replay(&self, id: &str, request: &mut ModelRequest) {
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
                    .entry(PROVIDER_STATE_ORIGIN_FIELD)
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
    pub fn record_origin(&self, id: &str, response: &mut ModelResponse) {
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

    pub fn models(&self) -> Vec<ModelInfo> {
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
    pub fn capabilities(&self, model: &str) -> Capabilities {
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
    pub fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&serde_json::Value) -> usize,
    ) -> Result<Option<usize>> {
        let (primary, selected) = self.primary_and_selected(request);
        let mut candidates = self.chain(&primary);
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
    /// Only the route the main loop tries first: the session's sticky route,
    /// else the primary. Fallbacks are not shaped.
    pub fn wire_shape(&self, request: &ModelRequest) -> Option<WireShape> {
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
    pub async fn close_session(&self, session_id: &str) -> Result<()> {
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
