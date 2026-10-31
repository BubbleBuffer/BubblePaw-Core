use super::*;
#[derive(Serialize, Deserialize)]
pub struct ReloadParams {
    pub server: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub features: Vec<String>,
    pub transport: String,
    pub input_limits: crate::input_files::InputLimits,
}
/// Host metadata with the currently accepted live model, access and context controls.
/// Embedders may supply additional projection fields.
pub type GetResult = Value;
pub type ReloadResult = crate::live_config::ReloadReport;
pub fn initialize(ctx: &HostCtx, _input: EmptyParams) -> InitializeResult {
    let legacy = [
        "sessions",
        "tools",
        "paragraphs",
        "steer",
        "follow_up",
        "cancel",
        "codemode",
        "environments",
        "outputs",
        "approvals",
        "events",
        "processes",
        "config_reload",
        "compact",
        "todo",
        "question",
        "question.answer_child",
        "child_events",
        "rewind",
        "context",
        "providers.list",
        "roles.set_main",
        "session.list.pagination",
        "child.cancel",
    ];
    let mut features = legacy
        .into_iter()
        .filter(|feature| METHODS.iter().any(|method| method.feature == Some(feature)))
        .collect::<Vec<_>>();
    for method in METHODS {
        if !features.contains(&method.name) {
            features.push(method.name);
        }
    }
    InitializeResult {
        protocol_version: "0.1".into(),
        features: features.into_iter().map(str::to_owned).collect(),
        transport: "stdio".into(),
        input_limits: ctx.input_limits,
    }
}
pub fn get(ctx: &HostCtx, input: ConfigGetParams) -> Result<GetResult, HostError> {
    let mut value = ctx.config.clone();
    if !value.is_object() {
        value = json!({});
    }
    value["session_limit"] = json!(64);
    value["input_limits"] = json!(
        ctx.input_files
            .as_ref()
            .map_or(ctx.input_limits, |files| files.limits())
    );
    if let Some(active) = ctx.active_providers() {
        let generation = active.snapshot();
        let settings = &generation.settings;
        value["model"] = json!(settings.main_model);
        value["provider"] = json!(crate::config::selected_main_provider(settings));
        value["read_only"] = json!(settings.approvals.read_only);
        if !value["context"].is_object() {
            value["context"] = json!({});
        }
        value["context"]["window_tokens"] = json!(settings.context_window_tokens);
        value["context"]["compact_threshold_tokens"] = json!(settings.compact_threshold_tokens);
        value["context"]["auto_summarize"] = json!(settings.auto_summarize);
    }
    match settings_list(ctx, input.key.as_deref())? {
        Some(list) => value["settings"] = list,
        None if input.key.is_some() => {
            return Err(HostError::unavailable(
                "Settings not available in this host",
            ));
        }
        None => {}
    }
    Ok(value)
}
/// The effective settings a running host holds, one entry per leaf, redacted
/// like `bubblepaw config`. Each entry names whether its key is live or
/// restart-only, and whether the file holds a restart-only change not yet running.
/// `key` selects one leaf or every leaf under a section. `None` without a config.
fn settings_list(ctx: &HostCtx, key: Option<&str>) -> Result<Option<Value>, HostError> {
    let (running, pending) = match (&ctx.live_config, ctx.active_providers()) {
        (Some(live), Some(providers)) => {
            let accepted = live.accepted();
            let startup = providers.startup_config();
            (
                startup.with_live_from(&accepted),
                accepted.restart_only_keys(startup),
            )
        }
        (_, Some(providers)) => (providers.snapshot().effective.clone(), Vec::new()),
        _ => return Ok(None),
    };
    let info = key
        .map(|key| {
            crate::config::redact::key_info(key)
                .ok_or_else(|| HostError::invalid(format!("Unknown config key: {key}")))
        })
        .transpose()?;
    let mut entries = running.setting_entries()?;
    if let Some(key) = key {
        let prefix = format!("{key}.");
        entries.retain(|entry| entry.key == key || entry.key.starts_with(&prefix));
        // A known leaf the file leaves unset still answers: null, with its liveness.
        if entries.is_empty() && info.is_some_and(|info| info.settable) {
            entries.push(crate::config::SettingEntry {
                key: key.into(),
                value: Value::Null,
                live: info.is_some_and(|info| info.live),
            });
        }
    }
    Ok(Some(Value::Array(
        entries
            .into_iter()
            .map(|entry| {
                let waiting = pending.iter().any(|path| {
                    entry.key == *path
                        || entry.key.starts_with(&format!("{path}."))
                        || path.starts_with(&format!("{}.", entry.key))
                });
                json!({
                    "key": entry.key,
                    "value": entry.value,
                    "live": entry.live,
                    "pending_restart": waiting,
                })
            })
            .collect(),
    )))
}
pub async fn set(ctx: &HostCtx, input: ConfigSetParams) -> Result<Value, HostError> {
    let setup = ctx
        .providers
        .as_ref()
        .filter(|setup| setup.0.host_config.is_some())
        .ok_or_else(|| HostError::unavailable("Config updates not available in this host"))?;
    let value = (!input.value.is_null()).then_some(&input.value);
    encode(setup.config_set(&input.key, value).await?)
}
pub async fn reload(ctx: &HostCtx, input: ReloadParams) -> Result<ReloadResult, HostError> {
    Ok(required(&ctx.live_config, "Config reload")?
        .reload(input.server.as_deref())
        .await)
}
pub(super) async fn command(
    ctx: &HostCtx,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    match method {
        // Protocol 0.1 ignored params entirely for this no-argument method.
        MethodId::Initialize => encode(initialize(ctx, EmptyParams {})),
        MethodId::ConfigGet => {
            let (ctx, input) = (ctx.clone(), decode(params)?);
            tokio::task::spawn_blocking(move || get(&ctx, input))
                .await
                .map_err(|e| HostError::internal(e.to_string()))?
        }
        MethodId::ConfigSet => set(ctx, decode(params)?).await,
        MethodId::ConfigReload => encode(reload(ctx, decode(params)?).await?),
        _ => unreachable!("config method table"),
    }
}
