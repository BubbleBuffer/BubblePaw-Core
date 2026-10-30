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
pub fn get(ctx: &HostCtx, _input: EmptyParams) -> Result<GetResult, HostError> {
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
    Ok(value)
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
        MethodId::ConfigReload => encode(reload(ctx, decode(params)?).await?),
        _ => unreachable!("config method table"),
    }
}
