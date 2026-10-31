//! One immutable provider/config generation per request, replaced as a unit.
use super::{ExtensionProvider, ProviderRegistry};
use crate::{
    config::{EffectiveConfig, LoadOptions, Settings, selected_main_provider},
    net::NetBackendSnapshot,
    no_provider::NoProviderConfigured,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Capabilities, CursorResets, DecisionProvider, DecisionRequest,
    DecisionResponse, ModelInfo, ModelRequest, ModelResponse, Provider, ProviderError,
    ProviderErrorKind, Store, TextCallback,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

#[path = "live_context.rs"]
mod context_binding;
#[path = "default_route.rs"]
mod default_route;
use default_route::default_route;

pub struct ProviderGeneration {
    pub registry: Arc<ProviderRegistry>,
    pub effective: EffectiveConfig,
    pub settings: Settings,
    pub net: Arc<NetBackendSnapshot>,
    id: u64,
    removed_models: BTreeSet<String>,
    known_routes: BTreeSet<String>,
    unavailable: Option<NoProviderConfigured>,
}

impl ProviderGeneration {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    /// Why the main model's route cannot serve a request, or `None` when it can.
    /// An unavailable generation still answers configuration and read methods.
    pub fn unavailable(&self) -> Option<&NoProviderConfigured> {
        self.unavailable.as_ref()
    }
}

/// A fully validated candidate, including credential-bound clients. No provider
/// requests or extension processes are started while preparing it.
pub struct PreparedProviderConfig {
    generation: ProviderGeneration,
}
impl PreparedProviderConfig {
    pub fn main_model(&self) -> &str {
        &self.generation.settings.main_model
    }
    /// Why the candidate's main route cannot serve a request, or `None`.
    pub fn unavailable(&self) -> Option<&NoProviderConfigured> {
        self.generation.unavailable()
    }
}

pub struct LiveProviderConfig {
    /// Owner of the model/provider section: one whole generation per publication.
    active: tokio::sync::watch::Sender<Arc<ProviderGeneration>>,
    hub: crate::settings_hub::SettingsHub,
    startup: EffectiveConfig,
    extensions: BTreeMap<String, Arc<dyn Provider>>,
    updates: Mutex<()>,
    next_id: AtomicU64,
    session_generations: Mutex<HashMap<String, u64>>,
    resets: CursorResets,
    image_store: Option<Store>,
    quota_host: Option<Arc<crate::quota::QuotaHost>>,
}
impl LiveProviderConfig {
    pub fn new(effective: EffectiveConfig) -> Result<Self> {
        let mut settings = effective.settings()?;
        settings.validate_provider_setup()?;
        let extensions = startup_extensions(&settings, Some(effective.runtime_environment()));
        let (registry, unavailable) =
            build_registry(&settings, &extensions, &|name| effective.environment(name))?;
        let (provider, _, model) = registry.route(&settings.main_model);
        let canonical = format!("{provider}/{model}");
        registry.checked_route(&canonical)?;
        settings = effective.settings_for_main(Some(&canonical))?;
        settings.main_model = canonical;
        let web = settings
            .web
            .net_config_with_env(&|name| effective.environment(name))?;
        let net = Arc::new(NetBackendSnapshot::from_config_with_env(
            web.as_ref(),
            &|name| effective.environment(name),
        )?);
        Ok(Self {
            hub: crate::settings_hub::SettingsHub::new(&settings),
            active: tokio::sync::watch::channel(Arc::new(ProviderGeneration {
                registry: Arc::new(registry),
                effective: effective.clone(),
                settings: settings.clone(),
                net,
                id: 0,
                removed_models: BTreeSet::new(),
                known_routes: known_routes(&settings),
                unavailable,
            }))
            .0,
            startup: effective,
            extensions,
            updates: Mutex::new(()),
            next_id: AtomicU64::new(1),
            session_generations: Mutex::default(),
            resets: CursorResets::new(),
            image_store: None,
            quota_host: None,
        })
    }
    pub fn snapshot(&self) -> Arc<ProviderGeneration> {
        self.active.borrow().clone()
    }
    /// Bind the concrete model and provider before asynchronous context
    /// preparation. Every operation on the returned provider retains this
    /// generation even if another configuration is published in the meantime.
    pub fn request_binding(self: &Arc<Self>) -> (String, Arc<dyn Provider>) {
        let generation = self.snapshot();
        let model = generation.settings.main_model.clone();
        (
            model,
            Arc::new(BoundProvider {
                live: self.clone(),
                generation,
            }),
        )
    }
    /// Hosts hold this from prepare through durable file replacement and publish.
    pub fn lock_update(&self) -> Result<MutexGuard<'_, ()>> {
        self.updates
            .lock()
            .map_err(|_| anyhow::anyhow!("provider configuration update lock poisoned"))
    }
    pub fn prepare(&self, effective: EffectiveConfig) -> Result<PreparedProviderConfig> {
        let candidate = effective.settings()?;
        // Validate disk settings even when the host keeps a nonlive startup field.
        candidate.validate_provider_setup()?;
        let projected = self.startup.with_live_from(&effective);
        let mut settings = projected.settings()?;
        anyhow::ensure!(
            settings
                .context_window_tokens
                .is_none_or(|window| window > settings.lcm.output_reserve_tokens),
            "Model context window must exceed the configured output reserve"
        );
        anyhow::ensure!(
            settings.compact_threshold_tokens > settings.lcm.output_reserve_tokens,
            "Context summarization threshold must exceed the configured output reserve"
        );
        let previous = self.snapshot();
        let mut known = known_routes(&settings);
        if let Some((prefix, _)) = settings.main_model.split_once('/')
            && previous.known_routes.contains(prefix)
            && !known.contains(prefix)
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Fatal,
                format!("Selected main provider route {prefix} is unavailable or was removed"),
            )
            .into());
        }
        known.extend(previous.known_routes.iter().cloned());
        let configured = configured_models(&settings);
        let mut removed = previous.removed_models.clone();
        removed.extend(
            configured_models(&previous.settings)
                .difference(&configured)
                .cloned(),
        );
        removed.retain(|model| !configured.contains(model));
        let (mut registry, unavailable) = build_registry(&settings, &self.extensions, &|name| {
            effective.environment(name)
        })?;
        // An applied candidate must not replace a working route with an unusable
        // one; only a host that has none yet may accept a configuration step
        // (such as saving a provider) that still leaves the main route open.
        if let Some(unavailable) = &unavailable
            && previous.unavailable.is_none()
        {
            return Err(anyhow::Error::new(unavailable.clone()));
        }
        registry.inherit_context_from(&previous.registry);
        if let Some(store) = &self.image_store {
            registry.set_usage_store(store.clone());
        }
        if let Some(host) = &self.quota_host {
            registry.set_quota_host(host.clone());
        }
        registry.set_removed_models(removed.clone());
        let (provider, _, model) = registry.route(&settings.main_model);
        let canonical = format!("{provider}/{model}");
        registry.checked_route(&canonical)?;
        settings = projected.settings_for_main(Some(&canonical))?;
        settings.main_model = canonical;
        let web = settings
            .web
            .net_config_with_env(&|name| effective.environment(name))?;
        let net = Arc::new(NetBackendSnapshot::from_config_with_env(
            web.as_ref(),
            &|name| effective.environment(name),
        )?);
        Ok(PreparedProviderConfig {
            generation: ProviderGeneration {
                registry: Arc::new(registry),
                effective,
                settings,
                net,
                id: self.next_id.fetch_add(1, Ordering::Relaxed),
                removed_models: removed,
                known_routes: known,
                unavailable,
            },
        })
    }
    /// Infallible publication after any file write. The host must hold
    /// `lock_update()` across prepare, durable write, and this operation.
    pub fn apply_prepared(&self, candidate: PreparedProviderConfig) -> Arc<ProviderGeneration> {
        let generation = Arc::new(candidate.generation);
        self.hub.publish(&generation.settings);
        self.active.send_replace(generation.clone());
        generation
    }
    pub fn reload(&self) -> Result<Arc<ProviderGeneration>> {
        let _update = self.lock_update()?;
        let effective = self.startup.reload()?;
        let candidate = self.prepare(effective)?;
        Ok(self.apply_prepared(candidate))
    }
    pub fn main_model(&self) -> String {
        self.snapshot().settings.main_model.clone()
    }
    pub fn load_options(&self) -> LoadOptions {
        self.startup.load_options()
    }
    pub fn startup_config(&self) -> &EffectiveConfig {
        &self.startup
    }
    fn prepare_cursor(&self, generation: &ProviderGeneration, request: &mut ModelRequest) {
        if request.session_id.is_empty() {
            return;
        }
        let previous = self
            .session_generations
            .lock()
            .expect("provider session lock poisoned")
            .insert(request.session_id.clone(), generation.id);
        if previous.is_some_and(|previous| previous != generation.id) {
            // Shared extensions keep their process, but a new serving generation
            // must rebuild any mirror. Publication never closes an in-flight
            // provider; the next step's cursor communicates the rebuild.
            self.resets.reset(&request.session_id, &mut request.history);
        } else {
            self.resets.apply(&request.session_id, &mut request.history);
        }
    }
}

struct BoundProvider {
    live: Arc<LiveProviderConfig>,
    generation: Arc<ProviderGeneration>,
}
#[async_trait]
impl Provider for BoundProvider {
    fn summary_fork_routes(&self, model: &str) -> Vec<String> {
        self.generation.registry.summary_fork_routes(model)
    }
    fn estimate_summary_fork_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&serde_json::Value) -> usize,
    ) -> Result<Option<usize>> {
        let mut request = request.clone();
        self.live.prepare_images(&self.generation, &mut request)?;
        self.generation
            .registry
            .estimate_summary_fork_tokens(&request, count)
    }
    fn supports_summary_fork(&self, model: &str) -> bool {
        self.generation.registry.supports_summary_fork(model)
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<serde_json::Value>> {
        let mut request = request.clone();
        self.live.prepare_images(&self.generation, &mut request)?;
        self.generation.registry.summary_fork_mapping(&request)
    }
    async fn generate_summary_fork(
        &self,
        mut request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        // Stateless fork dispatch must not advance the main session cursor.
        self.live.prepare_images(&self.generation, &mut request)?;
        self.generation
            .registry
            .generate_summary_fork(request, cancellation)
            .await
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&serde_json::Value) -> usize,
    ) -> Result<Option<usize>> {
        let mut request = request.clone();
        self.live.prepare_images(&self.generation, &mut request)?;
        self.generation
            .registry
            .estimate_input_tokens(&request, count)
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<bubblepaw_core::WireShape> {
        let mut request = request.clone();
        self.live
            .prepare_images(&self.generation, &mut request)
            .ok()?;
        self.generation.registry.wire_shape(&request)
    }
    fn models(&self) -> Vec<ModelInfo> {
        self.generation.registry.models()
    }
    fn capabilities(&self, model: &str) -> Capabilities {
        self.generation.registry.capabilities(model)
    }
    async fn generate(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.live.prepare_cursor(&self.generation, &mut request);
        self.live.prepare_images(&self.generation, &mut request)?;
        self.generation
            .registry
            .generate(request, on_text, cancellation)
            .await
    }
    async fn generate_decision(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.live.prepare_cursor(&self.generation, &mut request);
        self.live.prepare_images(&self.generation, &mut request)?;
        self.generation
            .registry
            .generate_decision(request, on_text, cancellation)
            .await
    }
    async fn close_session(&self, session_id: &str) -> Result<()> {
        self.live
            .session_generations
            .lock()
            .expect("provider session lock poisoned")
            .remove(session_id);
        self.live.resets.forget(session_id);
        self.generation.registry.close_session(session_id).await
    }
}

#[async_trait]
impl Provider for LiveProviderConfig {
    fn summary_fork_routes(&self, model: &str) -> Vec<String> {
        self.snapshot().registry.summary_fork_routes(model)
    }
    fn estimate_summary_fork_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&serde_json::Value) -> usize,
    ) -> Result<Option<usize>> {
        let generation = self.snapshot();
        let mut request = request.clone();
        self.prepare_images(&generation, &mut request)?;
        generation
            .registry
            .estimate_summary_fork_tokens(&request, count)
    }
    fn supports_summary_fork(&self, model: &str) -> bool {
        self.snapshot().registry.supports_summary_fork(model)
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<serde_json::Value>> {
        let generation = self.snapshot();
        let mut request = request.clone();
        self.prepare_images(&generation, &mut request)?;
        generation.registry.summary_fork_mapping(&request)
    }
    async fn generate_summary_fork(
        &self,
        mut request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let generation = self.snapshot();
        self.prepare_images(&generation, &mut request)?;
        generation
            .registry
            .generate_summary_fork(request, cancellation)
            .await
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&serde_json::Value) -> usize,
    ) -> Result<Option<usize>> {
        let generation = self.snapshot();
        let mut request = request.clone();
        self.prepare_images(&generation, &mut request)?;
        generation.registry.estimate_input_tokens(&request, count)
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<bubblepaw_core::WireShape> {
        let generation = self.snapshot();
        let mut request = request.clone();
        self.prepare_images(&generation, &mut request).ok()?;
        generation.registry.wire_shape(&request)
    }
    fn models(&self) -> Vec<ModelInfo> {
        self.snapshot().registry.models()
    }
    fn capabilities(&self, model: &str) -> Capabilities {
        self.snapshot().registry.capabilities(model)
    }
    async fn generate(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let generation = self.snapshot();
        self.prepare_cursor(&generation, &mut request);
        self.prepare_images(&generation, &mut request)?;
        generation
            .registry
            .generate(request, on_text, cancellation)
            .await
    }
    async fn generate_decision(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let generation = self.snapshot();
        self.prepare_cursor(&generation, &mut request);
        self.prepare_images(&generation, &mut request)?;
        generation
            .registry
            .generate_decision(request, on_text, cancellation)
            .await
    }
    async fn close_session(&self, session_id: &str) -> Result<()> {
        self.session_generations
            .lock()
            .expect("provider session lock poisoned")
            .remove(session_id);
        self.resets.forget(session_id);
        let generation = self.snapshot();
        generation.registry.close_session(session_id).await
    }
}
#[async_trait]
impl DecisionProvider for LiveProviderConfig {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        let generation = self.snapshot();
        generation.registry.decide(request, cancellation).await
    }
}

fn configured_models(settings: &Settings) -> BTreeSet<String> {
    settings
        .providers
        .iter()
        .flat_map(|(id, provider)| {
            provider
                .models
                .iter()
                .map(move |model| format!("{id}/{}", model.id))
        })
        .collect()
}
fn known_routes(settings: &Settings) -> BTreeSet<String> {
    settings
        .providers
        .iter()
        .filter(|(_, provider)| {
            matches!(
                provider.kind.as_deref().unwrap_or("chat-completions"),
                "codex" | "scripted" | "responses" | "chat-completions" | "anthropic-messages"
            )
        })
        .map(|(id, _)| id.clone())
        .chain(
            settings
                .extensions
                .iter()
                .map(|extension| extension.id.clone()),
        )
        .collect()
}
fn startup_extensions(
    settings: &Settings,
    inherited: Option<Vec<(String, String)>>,
) -> BTreeMap<String, Arc<dyn Provider>> {
    settings
        .extensions
        .iter()
        .map(|extension| {
            let provider = ExtensionProvider::new(extension.clone());
            let provider = match &inherited {
                Some(env) => provider.with_inherited_env(env.clone()),
                None => provider,
            };
            (
                extension.id.clone(),
                Arc::new(provider) as Arc<dyn Provider>,
            )
        })
        .collect()
}
pub fn build_provider_registry(settings: &Settings) -> Result<ProviderRegistry> {
    let (registry, unavailable) =
        build_registry(settings, &startup_extensions(settings, None), &|name| {
            std::env::var(name).ok()
        })?;
    match unavailable {
        Some(unavailable) => Err(unavailable.into()),
        None => Ok(registry),
    }
}
fn configure_routes(settings: &Settings, registry: &mut ProviderRegistry) {
    for (route, account) in settings.provider_accounts() {
        registry.set_account(route, account);
    }
    for id in registry.ids() {
        let endpoint = settings
            .providers
            .get(&id)
            .and_then(|provider| provider.base_url.as_deref())
            .map(|url| url.trim_end_matches('/').to_owned())
            .unwrap_or_else(|| "extension".into());
        let models = settings
            .providers
            .get(&id)
            .map(|provider| {
                provider
                    .models
                    .iter()
                    .map(|model| model.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        registry.configure_route(&id, endpoint, models);
        if let Some(provider) = settings.providers.get(&id) {
            for model in &provider.models {
                if let Some(enabled) = model.image_input {
                    registry.set_image_input(&id, &model.id, enabled);
                }
            }
        }
    }
}

fn build_registry(
    settings: &Settings,
    extensions: &BTreeMap<String, Arc<dyn Provider>>,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Result<(ProviderRegistry, Option<NoProviderConfigured>)> {
    settings.validate_provider_setup()?;
    let default = selected_main_provider(settings);
    let (default_provider, unavailable) = default_route(settings, extensions, environment)?;
    let mut registry = ProviderRegistry::new(default, default_provider).with_strict_routes();
    for extension in &settings.extensions {
        if extension.id != default {
            registry.insert(
                extension.id.clone(),
                extensions
                    .get(&extension.id)
                    .context("Missing startup extension")?
                    .clone(),
            )?;
        }
    }
    settings.register_providers_with_env(&mut registry, environment)?;
    configure_routes(settings, &mut registry);
    let primary = if settings.main_model.contains('/') {
        settings.main_model.clone()
    } else {
        format!("{}/{}", settings.main_provider, settings.main_model)
    };
    registry.set_model_fallbacks(primary, settings.main_fallbacks.clone())?;
    Ok((registry, unavailable))
}

#[cfg(test)]
#[path = "live_binding_tests.rs"]
mod binding_tests;
