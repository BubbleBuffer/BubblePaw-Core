//! One immutable provider/config generation per request, replaced as a unit.
use super::{ExtensionProvider, ProviderRegistry, ResponsesProvider, ScriptedProvider};
use crate::{
    config::{EffectiveConfig, LoadOptions, Settings, is_opencode_route, selected_main_provider},
    net::NetBackendSnapshot,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Capabilities, CursorResets, DecisionProvider, DecisionRequest,
    DecisionResponse, ModelInfo, ModelRequest, ModelResponse, Provider, ProviderError,
    ProviderErrorKind, Store, TextCallback,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

#[path = "live_context.rs"]
mod context_binding;

pub struct ProviderGeneration {
    pub registry: Arc<ProviderRegistry>,
    pub effective: EffectiveConfig,
    pub settings: Settings,
    pub net: Arc<NetBackendSnapshot>,
    id: u64,
    removed_models: BTreeSet<String>,
    known_routes: BTreeSet<String>,
}

impl ProviderGeneration {
    pub(crate) fn id(&self) -> u64 {
        self.id
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
}

pub struct LiveProviderConfig {
    active: RwLock<Arc<ProviderGeneration>>,
    startup: EffectiveConfig,
    startup_settings: Settings,
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
        let registry = build_registry(&settings, &extensions, &|name| effective.environment(name))?;
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
            active: RwLock::new(Arc::new(ProviderGeneration {
                registry: Arc::new(registry),
                effective: effective.clone(),
                settings: settings.clone(),
                net,
                id: 0,
                removed_models: BTreeSet::new(),
                known_routes: known_routes(&settings),
            })),
            startup: effective,
            startup_settings: settings,
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
        self.active
            .read()
            .expect("provider generation lock poisoned")
            .clone()
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
        let mut settings = self.startup_settings.with_live_provider_settings(candidate);
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
        let mut registry = build_registry(&settings, &self.extensions, &|name| {
            effective.environment(name)
        })?;
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
        settings = self
            .startup_settings
            .with_live_provider_settings(effective.settings_for_main(Some(&canonical))?);
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
            },
        })
    }
    /// Infallible publication after any file write. The host must hold
    /// `lock_update()` across prepare, durable write, and this operation.
    pub fn apply_prepared(&self, candidate: PreparedProviderConfig) -> Arc<ProviderGeneration> {
        let mut active = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = Arc::new(candidate.generation);
        *active = generation.clone();
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
    build_registry(settings, &startup_extensions(settings, None), &|name| {
        std::env::var(name).ok()
    })
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
) -> Result<ProviderRegistry> {
    settings.validate_provider_setup()?;
    let default = selected_main_provider(settings);
    let default_provider: Arc<dyn Provider> = if let Some(extension) = settings
        .extensions
        .iter()
        .find(|extension| extension.id == default)
    {
        extensions
            .get(&extension.id)
            .context("Missing startup extension")?
            .clone()
    } else {
        let transport = settings
            .providers
            .get(default)
            .context("model.provider is not configured")?;
        let base = transport.base_url.clone();
        if transport.kind.as_deref() != Some("scripted")
            && base.as_deref().is_none_or(|base| base.trim().is_empty())
        {
            bail!("Selected provider route {default} is disabled: base_url is empty");
        }
        match transport.kind.as_deref().unwrap_or("chat-completions") {
            "scripted" => Arc::new(ScriptedProvider::demo(!settings.approvals.read_only)),
            "codex" => {
                let auth = transport
                    .auth_file
                    .clone()
                    .or_else(|| {
                        environment("CODEX_HOME").map(|p| PathBuf::from(p).join("auth.json"))
                    })
                    .or_else(|| {
                        environment("USERPROFILE")
                            .or_else(|| environment("HOME"))
                            .map(|p| PathBuf::from(p).join(".codex/auth.json"))
                    })
                    .context("choose --auth-file for Codex credentials")?;
                let mut provider = ResponsesProvider::codex_auth(
                    auth,
                    base.context("Codex provider needs base_url")?,
                )?;
                if let Some(effort) = transport
                    .reasoning
                    .get("reasoning_effort")
                    .and_then(toml::Value::as_str)
                {
                    provider = provider.with_reasoning_effort(effort);
                }
                Arc::new(provider)
            }
            "responses" | "chat-completions" | "anthropic-messages" => {
                let key_env = transport
                    .key_env
                    .as_ref()
                    .context("Provider needs key_env")?;
                let key = environment(key_env).ok_or_else(|| {
                    anyhow::anyhow!("{key_env} is not set or is not UTF-8; choose --provider codex or demo explicitly")
                })?;
                anyhow::ensure!(
                    reqwest::header::HeaderValue::from_str(&key).is_ok(),
                    "Invalid credential header for provider {default}"
                );
                let base = base.context("Provider needs base_url")?;
                if transport.kind.as_deref() == Some("responses") {
                    let mut provider = ResponsesProvider::api_key(key, base)?;
                    if is_opencode_route(default) {
                        provider = provider.with_opencode_route();
                    }
                    if let Some(effort) = transport
                        .reasoning
                        .get("reasoning_effort")
                        .and_then(toml::Value::as_str)
                    {
                        provider = provider.with_reasoning_effort(effort);
                    }
                    Arc::new(provider)
                } else if transport.kind.as_deref() == Some("anthropic-messages") {
                    let mut provider = super::MessagesProvider::api_key(key, base)?;
                    if is_opencode_route(default) {
                        provider = provider.with_opencode_route();
                    }
                    Arc::new(provider)
                } else {
                    let mut provider = super::ChatCompletionsProvider::api_key(key, base)?
                        .with_reasoning_fields(serde_json::to_value(&transport.reasoning)?)?
                        .with_reasoning_replay(transport.reasoning_replay())?;
                    if is_opencode_route(default) {
                        provider = provider.with_opencode_route();
                    }
                    Arc::new(provider)
                }
            }
            _ => bail!("model.provider must select an LLM provider"),
        }
    };
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
    Ok(registry)
}

#[cfg(test)]
mod binding_tests {
    use super::*;
    use crate::config::Override;
    use bubblepaw_core::{ProviderError, ProviderErrorKind, ToolDefinition};
    use serde_json::json;
    use std::path::Path;

    fn scripted_config(workspace: &Path, provider: &str, model: &str) -> EffectiveConfig {
        let providers: toml::Value = format!("[{provider}]\nkind='scripted'\n").parse().unwrap();
        EffectiveConfig::load(LoadOptions {
            workspace: workspace.to_owned(),
            home: None,
            custom_file: None,
            env: Vec::new(),
            cli: vec![
                Override::replace("providers", providers),
                Override::set(&format!("model.main={model}")).unwrap(),
                Override::set(&format!("model.provider={provider}")).unwrap(),
                Override::set("approvals.read_only=true").unwrap(),
            ],
        })
        .unwrap()
    }

    fn scripted_request(model: &str) -> ModelRequest {
        ModelRequest {
            model: model.to_owned(),
            tools: vec![ToolDefinition {
                name: "js".into(),
                description: "Synthetic code surface; calls are never executed".into(),
                input_schema: json!({"type":"object"}),
                read_only: true,
                approval: Default::default(),
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn retained_request_binding_keeps_its_model_and_provider_generation() {
        let directory = tempfile::tempdir().unwrap();
        let initial = scripted_config(directory.path(), "demo", "first");
        let live = Arc::new(LiveProviderConfig::new(initial).unwrap());
        let (old_model, old_provider) = live.request_binding();
        assert_eq!(old_model, "demo/first");

        let next = scripted_config(directory.path(), "replacement", "second");
        {
            let _update = live.lock_update().unwrap();
            live.apply_prepared(live.prepare(next).unwrap());
        }
        let (new_model, new_provider) = live.request_binding();
        assert_eq!(new_model, "replacement/second");
        assert_eq!(old_provider.models()[0].id, "demo/scripted");
        assert_eq!(new_provider.models()[0].id, "replacement/scripted");

        // Context preparation may finish after the old route is removed. The
        // retained binding still dispatches using its own matching registry.
        let reply = old_provider
            .generate(
                scripted_request(&old_model),
                Arc::new(|_| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        let error = live
            .generate(
                scripted_request(&old_model),
                Arc::new(|_| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            ProviderError::of(&error).map(|error| &error.kind),
            Some(&ProviderErrorKind::Fatal)
        );
        let reply = new_provider
            .generate(
                scripted_request(&new_model),
                Arc::new(|_| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
    }

    #[tokio::test]
    async fn account_reload_preserves_the_inflight_generation_attribution() {
        let directory = tempfile::tempdir().unwrap();
        let mut options = scripted_config(directory.path(), "demo", "model").load_options();
        options
            .cli
            .push(Override::set("providers.demo.account=original").unwrap());
        let live = Arc::new(
            LiveProviderConfig::new(EffectiveConfig::load(options.clone()).unwrap()).unwrap(),
        );
        let (model, old) = live.request_binding();
        options
            .cli
            .push(Override::set("providers.demo.account=replacement").unwrap());
        {
            let _update = live.lock_update().unwrap();
            live.apply_prepared(
                live.prepare(EffectiveConfig::load(options).unwrap())
                    .unwrap(),
            );
        }
        let (_, new) = live.request_binding();
        let (result, rows) = bubblepaw_core::capture_request_usage(
            "session",
            None,
            bubblepaw_core::RequestKind::Main,
            async {
                old.generate(
                    scripted_request(&model),
                    Arc::new(|_| {}),
                    CancellationToken::new(),
                )
                .await?;
                new.generate(
                    scripted_request(&model),
                    Arc::new(|_| {}),
                    CancellationToken::new(),
                )
                .await?;
                Ok::<_, anyhow::Error>(())
            },
        )
        .await;
        result.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].account, "original");
        assert_eq!(rows[1].account, "replacement");
        assert!(rows.iter().all(|row| row.provider == "demo"));
    }

    #[tokio::test]
    async fn initial_vendor_namespace_uses_default_and_canonical_role_references() {
        let directory = tempfile::tempdir().unwrap();
        let config = scripted_config(directory.path(), "demo", "vendor/model");
        let live = Arc::new(LiveProviderConfig::new(config).unwrap());
        let (model, provider) = live.request_binding();
        assert_eq!(model, "demo/vendor/model");
        assert!(
            live.snapshot()
                .settings
                .roles
                .summary
                .models
                .contains(&model)
        );
        // No built-in decider: it stays empty until configured.
        assert!(live.snapshot().settings.roles.decider.models.is_empty());
        provider
            .generate(
                scripted_request(&model),
                Arc::new(|_| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let next = scripted_config(directory.path(), "replacement", "vendor/next");
        {
            let _update = live.lock_update().unwrap();
            live.apply_prepared(live.prepare(next).unwrap());
        }
        let (model, provider) = live.request_binding();
        assert_eq!(model, "replacement/vendor/next");
        assert!(
            live.snapshot()
                .settings
                .roles
                .summary
                .models
                .contains(&model)
        );
        provider
            .generate(
                scripted_request(&model),
                Arc::new(|_| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let error = live
            .generate(
                scripted_request("vendor/next"),
                Arc::new(|_| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            ProviderError::of(&error).map(|error| &error.kind),
            Some(&ProviderErrorKind::Fatal)
        );
    }

    #[test]
    fn removed_main_route_cannot_become_a_vendor_namespace_on_later_reload() {
        let directory = tempfile::tempdir().unwrap();
        let initial = scripted_config(directory.path(), "demo", "first");
        let live = LiveProviderConfig::new(initial).unwrap();
        {
            let _update = live.lock_update().unwrap();
            live.apply_prepared(
                live.prepare(scripted_config(directory.path(), "replacement", "second"))
                    .unwrap(),
            );
        }
        let removed_main = scripted_config(directory.path(), "replacement", "demo/first");
        let serving = live.snapshot();
        let error = live
            .prepare(removed_main.clone())
            .err()
            .expect("removed route must reject");
        assert_eq!(
            ProviderError::of(&error).map(|error| &error.kind),
            Some(&ProviderErrorKind::Fatal)
        );
        assert!(error.to_string().contains("route demo"));
        assert!(Arc::ptr_eq(&serving, &live.snapshot()));
        {
            let _update = live.lock_update().unwrap();
            live.apply_prepared(
                live.prepare(scripted_config(directory.path(), "replacement", "third"))
                    .unwrap(),
            );
        }
        assert!(live.prepare(removed_main).is_err());
        assert!(
            live.prepare(scripted_config(directory.path(), "demo", "first"))
                .is_ok()
        );
    }
}
