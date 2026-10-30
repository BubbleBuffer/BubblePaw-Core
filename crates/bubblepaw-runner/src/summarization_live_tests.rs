//! Accepted disk reloads reach existing context consumers without rebuilding them.
use super::*;
use crate::{
    config::{EffectiveConfig, LoadOptions},
    provider::LiveProviderConfig,
};

#[tokio::test]
async fn reload_changes_threshold_auto_and_model_window_but_retains_lcm() -> Result<()> {
    let root = tempfile::tempdir()?;
    let file = root.path().join("config.toml");
    let write = |model: &str, controls: &str| -> Result<()> {
        std::fs::write(
            &file,
            format!("[model]\nmain='codex/{model}'\n[context]\n{controls}"),
        )?;
        Ok(())
    };
    std::fs::write(
        root.path().join("models_cache.json"),
        json!({"models":[
            {"slug":"first","context_window":120000},
            {"slug":"second","context_window":180000}
        ]})
        .to_string(),
    )?;
    std::fs::write(
        root.path().join("auth.json"),
        json!({"tokens":{"access_token":"synthetic","account_id":"fixture"}}).to_string(),
    )?;
    write("first", "compact_threshold_tokens=200000\n")?;
    let effective = EffectiveConfig::load(LoadOptions {
        workspace: root.path().into(),
        home: None,
        custom_file: Some(file.clone()),
        env: vec![(
            "CODEX_HOME".into(),
            root.path().to_string_lossy().into_owned(),
        )],
        cli: Vec::new(),
    })?;
    let live = Arc::new(LiveProviderConfig::new(effective)?);
    let store = Store::in_memory()?;
    let session = store.create_session("live context")?.id;
    let context = SummarizingContext::new(store, live.clone(), SummaryConfig::default(), None)?
        .with_live_config(live.clone());
    assert_eq!(
        context
            .config_for(&session, &live.main_model())
            .context_window,
        Some(120000)
    );
    assert_eq!(
        context.config_for(&session, &live.main_model()).threshold(),
        104000
    );
    write(
        "second",
        "compact_threshold_tokens=150000\nauto_summarize=false\n[context.lcm]\noutput_reserve_tokens=30000\n",
    )?;
    live.reload()?;
    let config = context.config_for(&session, &live.main_model());
    assert_eq!(live.main_model(), "codex/second");
    assert_eq!(config.context_window, Some(180000));
    assert_eq!(config.threshold(), 134000);
    assert_eq!(config.lcm.output_reserve_tokens, 16000);
    assert!(!context.automatic());
    let request = ModelRequest {
        model: live.main_model(),
        messages: vec![SummarizingContext::synthetic(
            &session,
            Role::User,
            "x ".repeat(200000),
        )],
        ..Default::default()
    };
    // Automatic admission is skipped with the ordinary history left intact.
    let (_, prepared) = prepare_main(&context, live.clone(), &session, request.clone()).await?;
    assert_eq!(prepared.request.messages, request.messages);
    context.validate_final_request(&session, &request, live.clone())?;
    write(
        "second",
        "window_tokens=100000\ncompact_threshold_tokens=90000\nauto_summarize=true\n",
    )?;
    live.reload()?;
    let config = context.config_for(&session, &live.main_model());
    assert_eq!(config.context_window, Some(100000));
    assert_eq!(config.threshold(), 74000);
    assert!(context.automatic());
    assert!(
        context
            .validate_final_request(&session, &request, live.clone())
            .is_err()
    );
    // Removing an explicit window resumes metadata lookup for the current model.
    write("second", "compact_threshold_tokens=190000\n")?;
    live.reload()?;
    assert_eq!(
        context
            .config_for(&session, &live.main_model())
            .context_window,
        Some(180000)
    );
    Ok(())
}
