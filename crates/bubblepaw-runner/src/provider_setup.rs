//! Provider configuration shared by stdio and HTTP hosts.
use crate::config::{EffectiveConfig, LoadOptions, ProviderSettings};
use crate::provider::LiveProviderConfig;
use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path as FilePath, PathBuf},
    sync::{Arc, Mutex},
};
use toml_edit::{DocumentMut, Item, Table, value};

mod input;
mod probe;
mod save;
#[cfg(all(test, feature = "web"))]
pub(crate) use input::loopback_url;
#[cfg(all(test, feature = "web"))]
pub(crate) use probe::{ConfirmedHost, auth_status, codex_auth_file, confirmed_host, model_url};

pub(crate) const KINDS: &[&str] = &[
    "codex",
    "scripted",
    "responses",
    "chat-completions",
    "anthropic-messages",
    "decisions",
    "openai-decisions",
    "openrouter-decisions",
];
#[derive(Clone)]
pub struct ProviderSetup(pub(crate) Arc<SetupState>);

pub(crate) struct SetupState {
    pub(crate) target: PathBuf,
    pub(crate) layer: &'static str,
    pub(crate) inputs: LoadOptions,
    pub(crate) launched: Value,
    pub(crate) launched_main: String,
    pub(crate) live: Option<Arc<LiveProviderConfig>>,
    pub(crate) host_config: Option<Arc<crate::live_config::LiveConfig>>,
    // Serialize reads and writes within this host; a second process is not locked.
    pub(crate) files: Mutex<()>,
}

impl ProviderSetup {
    /// Inputs must be captured before startup rewrites CLI fields from settings.
    pub fn new(mut inputs: LoadOptions, launched: &EffectiveConfig) -> Result<Self> {
        // Public load_options intentionally redacts process inputs. Setup must
        // retain the invoking environment when rebuilding a saved candidate.
        inputs.env = launched.runtime_environment();
        let custom = inputs.custom_file.clone().or_else(|| {
            inputs
                .env
                .iter()
                .find(|(key, _)| key == "BUBBLEPAW_CONFIG")
                .map(|(_, path)| PathBuf::from(path))
        });
        let (target, layer) = match custom {
            Some(path) => (path, "custom"),
            None => (
                inputs
                    .home
                    .as_ref()
                    .context("Provider setup needs a home directory")?
                    .join(".bubblepaw/config.toml"),
                "user",
            ),
        };
        let target = absolute(&target)?;
        if layer == "custom" {
            inputs.custom_file = Some(target.clone());
        }
        // Use absolute layer paths for stable provenance and relative auth-file resolution.
        inputs.workspace = absolute(&inputs.workspace)?;
        inputs.home = inputs.home.as_deref().map(absolute).transpose()?;
        let launched_main = main_model(launched)?;
        let launched = snapshot(launched)?;
        Ok(Self(Arc::new(SetupState {
            target,
            layer,
            inputs,
            launched,
            launched_main,
            live: None,
            host_config: None,
            files: Mutex::new(()),
        })))
    }
    /// Bind setup writes and reloads to the running host's provider snapshots.
    pub fn with_live(mut self, live: Arc<LiveProviderConfig>) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("bind live providers before cloning setup")
            .live = Some(live);
        self
    }
    pub fn with_host_config(mut self, live: Arc<crate::live_config::LiveConfig>) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("bind host config before cloning setup")
            .host_config = Some(live);
        self
    }
}

fn absolute(path: &FilePath) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    })
}

#[derive(Debug)]
pub(crate) struct ApiError(
    #[cfg_attr(not(feature = "web"), allow(dead_code))] pub(crate) StatusCode,
    pub(crate) String,
);
pub(crate) type ApiResult<T> = std::result::Result<T, ApiError>;
pub(crate) fn bad() -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "Invalid provider configuration".into(),
    )
}
pub(crate) fn conflict() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "A later configuration layer overrides the saved settings".into(),
    )
}
pub(crate) fn io_error() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Cannot read or write provider configuration".into(),
    )
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.1)
    }
}
impl std::error::Error for ApiError {}

pub(crate) fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
pub(crate) fn provider_json(provider: &ProviderSettings) -> Value {
    let kind = provider.kind.as_deref().unwrap_or("chat-completions");
    let default_replay = if kind == "chat-completions" {
        bubblepaw_core::ReasoningReplay::default()
    } else {
        bubblepaw_core::ReasoningReplay::None
    };
    json!({
        "kind": kind,
        "base_url": provider.base_url,
        "key_env": provider.key_env,
        "auth_file": provider.auth_file,
        "reasoning": provider.reasoning,
        "models": provider.models.iter().map(|model| json!({
            "id": model.id,
            "image_input": model.image_input.unwrap_or(matches!(kind, "codex" | "responses" | "chat-completions" | "anthropic-messages")),
            "reasoning_replay": model.reasoning_replay.unwrap_or(default_replay),
        })).collect::<Vec<_>>(),
    })
}
pub(crate) fn main_model(config: &EffectiveConfig) -> Result<String> {
    let settings = config.settings()?;
    Ok(settings.main_model)
}
fn snapshot(config: &EffectiveConfig) -> Result<Value> {
    let settings = config.settings()?;
    let mut providers = BTreeMap::new();
    for (name, provider) in &settings.providers {
        let mut entry = provider_json(provider);
        if let Some(path) = &provider.auth_file {
            entry["auth_file"] = json!(absolute(path)?);
        }
        providers.insert(name.clone(), entry);
    }
    Ok(json!({
        "providers": providers,
        "main": main_model(config)?,
    }))
}
fn provider_source(config: &EffectiveConfig, name: &str) -> String {
    fn visit<'a>(
        config: &'a EffectiveConfig,
        path: &str,
        node: &toml::Value,
        sources: &mut Vec<&'a str>,
    ) {
        if let Some(source) = config.source(path) {
            sources.push(source);
        }
        if let Some(table) = node.as_table() {
            for (key, val) in table {
                visit(config, &format!("{path}.{key}"), val, sources);
            }
        }
    }
    let path = format!("providers.{name}");
    let mut sources = Vec::new();
    if let Some(node) = config.value(&path) {
        visit(config, &path, node, &mut sources);
    }
    sources
        .into_iter()
        .filter_map(|source| source.split_once(": "))
        .max_by_key(|(layer, _)| match *layer {
            "custom" => 3,
            "project" => 2,
            "user" => 1,
            _ => 0,
        })
        .map_or_else(|| "defaults".into(), |(_, path)| path.to_owned())
}

impl SetupState {
    pub(crate) fn lower(&self) -> ApiResult<EffectiveConfig> {
        // Both permitted write targets sit immediately above defaults. Custom
        // configuration replaces user/project files; user is the first file layer.
        let mut inputs = self.inputs.clone();
        inputs.custom_file = Some(self.target.clone());
        inputs.env.clear();
        inputs.cli.clear();
        EffectiveConfig::load_with_file(inputs, &self.target, "").map_err(|_| bad())
    }
    pub(crate) fn document(&self) -> ApiResult<DocumentMut> {
        match std::fs::read_to_string(&self.target) {
            Ok(text) => text.parse().map_err(|_| io_error()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
            Err(_) => Err(io_error()),
        }
    }
    pub(crate) fn load(&self) -> ApiResult<EffectiveConfig> {
        EffectiveConfig::load(self.inputs.clone()).map_err(|_| bad())
    }
    pub(crate) fn payload(&self) -> ApiResult<Value> {
        let config = self.load()?;
        let doc = self.document()?;
        self.payload_for(&config, &doc, false, None)
    }
    pub(crate) fn payload_for(
        &self,
        config: &EffectiveConfig,
        doc: &DocumentMut,
        applying: bool,
        applied_main: Option<&str>,
    ) -> ApiResult<Value> {
        let settings = config.settings().map_err(|_| bad())?;
        let lower = self.lower()?;
        let providers = settings
            .providers
            .iter()
            .map(|(name, provider)| {
                let mut entry = provider_json(provider);
                entry["name"] = json!(name);
                entry["key_present"] = json!(
                    provider
                        .key_env
                        .as_ref()
                        .and_then(|name| config.environment(name))
                        .is_some_and(|value| !value.is_empty())
                );
                // File provenance can differ by field in a merged provider table.
                entry["source"] = json!(provider_source(config, name));
                entry["inherited_fields"] = json!(
                    lower
                        .value(&format!("providers.{name}"))
                        .and_then(toml::Value::as_table)
                        .map(|fields| fields.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                );
                entry["editable"] = json!(true);
                entry["deletable"] = json!(target_provider(doc, name));
                entry
            })
            .collect::<Vec<_>>();
        let active = self.live.as_ref().map(|live| live.snapshot());
        let active_config = active.as_ref().map(|generation| &generation.effective);
        let reload_needed = !(applying && self.live.is_some())
            && active_config
                .map(|active| !active.same_provider_config(config))
                .unwrap_or_else(|| snapshot(config).ok().as_ref() != Some(&self.launched));
        let active_main = if let Some(main) = applied_main {
            main.to_owned()
        } else {
            active
                .as_ref()
                .map(|generation| generation.settings.main_model.clone())
                .unwrap_or_else(|| self.launched_main.clone())
        };
        Ok(json!({
            "target": {"path": self.target, "layer": self.layer, "exists": applying || self.target.exists()},
            "kinds": KINDS, "main": main_model(config).map_err(|_| bad())?,
            "launched_main": self.launched_main,
            "active_main": active_main,
            // Why the active main route cannot serve a request, or null when it can.
            "main_unavailable": active
                .as_ref()
                .and_then(|generation| generation.unavailable())
                .map(ToString::to_string),
            "reload_needed": reload_needed,
            "allowed_key_envs": settings.web.provider_key_envs,
            "restart_needed": self.live.as_ref().map(|live| config.restart_needed(live.startup_config()))
                .unwrap_or_else(|| snapshot(config).ok().as_ref() != Some(&self.launched)),
            "providers": providers,
        }))
    }
    pub(crate) fn candidate(&self, doc: &DocumentMut) -> ApiResult<EffectiveConfig> {
        EffectiveConfig::load_with_file(self.inputs.clone(), &self.target, &doc.to_string())
            .map_err(|_| bad())
    }
    pub(crate) fn write(&self, doc: &DocumentMut) -> ApiResult<()> {
        let parent = self.target.parent().ok_or_else(io_error)?;
        std::fs::create_dir_all(parent).map_err(|_| io_error())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| io_error())?;
        if let Ok(metadata) = std::fs::metadata(&self.target) {
            file.as_file()
                .set_permissions(metadata.permissions())
                .map_err(|_| io_error())?;
        }
        file.write_all(doc.to_string().as_bytes())
            .map_err(|_| io_error())?;
        file.as_file().sync_all().map_err(|_| io_error())?;
        file.persist(&self.target).map_err(|_| io_error())?;
        crate::live_config::diag_note(format!("write file len={}", doc.to_string().len()));
        Ok(())
    }
    pub(crate) fn write_apply(
        &self,
        doc: &DocumentMut,
        candidate: EffectiveConfig,
    ) -> ApiResult<Value> {
        let prepared = self
            .live
            .as_ref()
            .map(|live| live.prepare(candidate.clone()))
            .transpose()
            .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
        let mut reply = self.payload_for(
            &candidate,
            doc,
            true,
            prepared.as_ref().map(|candidate| candidate.main_model()),
        )?;
        if let Some(prepared) = &prepared {
            // The reply describes the configuration about to be published.
            reply["main_unavailable"] = json!(prepared.unavailable().map(ToString::to_string));
        }
        // All fallible preparation precedes the file replacement. Publication is infallible.
        self.write(doc)?;
        if let (Some(live), Some(prepared)) = (&self.live, prepared) {
            live.apply_prepared(prepared);
        }
        Ok(reply)
    }
}
pub(crate) fn target_provider(doc: &DocumentMut, name: &str) -> bool {
    doc.get("providers")
        .and_then(|item| item.get(name))
        .is_some_and(|item| item.is_table_like())
}
pub(crate) fn table<'a>(
    doc: &'a mut DocumentMut,
    name: &str,
) -> ApiResult<&'a mut dyn toml_edit::TableLike> {
    if !doc.contains_key(name) {
        doc.insert(name, Item::Table(Table::new()));
    }
    doc.get_mut(name)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(bad)
}
impl ProviderSetup {
    /// Return the same redacted provider snapshot as the HTTP setup endpoint.
    pub fn list(&self) -> Result<Value> {
        let _guard = self.0.files.lock().map_err(|_| io_error())?;
        Ok(self.0.payload()?)
    }
    /// Persist and live-apply the main model using the host's normal layering
    /// checks and atomic configuration replacement. Does not call a provider.
    pub async fn set_main(&self, model: &str) -> Result<Value> {
        Ok(self.set_main_inner(model).await?)
    }
    pub(crate) async fn set_main_inner(&self, model: &str) -> ApiResult<Value> {
        let _host_update = match &self.0.host_config {
            Some(host) => Some(host.lock_update().await),
            None => None,
        };
        let (provider, id) = model.split_once('/').ok_or_else(bad)?;
        if !valid_name(provider)
            || id.is_empty()
            || id.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(bad());
        }
        let _guard = self.0.files.lock().map_err(|_| io_error())?;
        let _update = self
            .0
            .live
            .as_ref()
            .map(|live| live.lock_update())
            .transpose()
            .map_err(|_| io_error())?;
        if !self
            .0
            .load()?
            .settings()
            .map_err(|_| bad())?
            .providers
            .contains_key(provider)
        {
            return Err(bad());
        }
        let mut doc = self.0.document()?;
        table(&mut doc, "model")?.insert("main", value(model));
        let candidate = self.0.candidate(&doc)?;
        if main_model(&candidate).map_err(|_| bad())? != model {
            return Err(conflict());
        }
        self.0.write_apply(&doc, candidate)
    }
}
