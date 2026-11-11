//! Model roles and reasoning effort settings.
use super::*;

#[test]
fn role_references_main_summary_and_llm_summary_resolve() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("roles.summary=['opencode/fixture','main']").unwrap(),
        Override::set("roles.chapter='summary'").unwrap(),
        Override::set("roles.decider='llm/summary'").unwrap(),
    ];
    let roles = EffectiveConfig::load(opts)
        .unwrap()
        .settings()
        .unwrap()
        .roles;
    assert_eq!(
        roles.summary.models,
        ["opencode/fixture", "codex/gpt-6-luna"]
    );
    assert_eq!(roles.chapter.models, roles.summary.models);
    assert_eq!(
        roles.decider.models,
        ["llm/opencode/fixture", "llm/codex/gpt-6-luna"]
    );
    assert_eq!(roles.summary.repeats, Some(1));
    assert_eq!(roles.summary.threshold, Some(0.0));
}

#[test]
fn cycles_and_bad_role_settings_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    for entries in [
        vec!["roles.summary='chapter'", "roles.chapter='summary'"],
        vec!["roles.decider.repeats=0"],
        vec!["roles.approver.threshold=1.1"],
        vec!["roles.summary=[]"],
        vec!["roles.summary='missing'"],
    ] {
        let mut opts = options(root.path());
        opts.cli = entries
            .into_iter()
            .map(|entry| Override::set(entry).unwrap())
            .collect();
        assert!(EffectiveConfig::load(opts).is_err());
    }
}

#[test]
fn role_reasoning_effort_layers_validate_and_keep_defaults() {
    let root = tempfile::tempdir().unwrap();
    let defaults = EffectiveConfig::load(options(root.path()))
        .unwrap()
        .settings()
        .unwrap();
    for role in [
        &defaults.roles.summary,
        &defaults.roles.chapter,
        &defaults.roles.decider,
        &defaults.roles.approver,
    ] {
        assert_eq!(role.reasoning_effort, ReasoningEffort::Low);
    }
    let custom = root.path().join("effort.toml");
    std::fs::write(&custom, "[roles.summary]\nreasoning_effort='medium'\n").unwrap();
    let mut opts = options(root.path());
    opts.custom_file = Some(custom.clone());
    let file = EffectiveConfig::load(opts).unwrap();
    assert_eq!(
        file.settings().unwrap().roles.summary.reasoning_effort,
        ReasoningEffort::Medium
    );
    assert_eq!(
        file.source("roles.summary.reasoning_effort"),
        Some(format!("custom: {}", custom.display()).as_str())
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(custom.clone());
    opts.env = vec![(
        "BUBBLEPAW_ROLES__SUMMARY__REASONING_EFFORT".into(),
        "high".into(),
    )];
    let env = EffectiveConfig::load(opts).unwrap();
    assert_eq!(
        env.settings().unwrap().roles.summary.reasoning_effort,
        ReasoningEffort::High
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(custom);
    opts.env = vec![(
        "BUBBLEPAW_ROLES__SUMMARY__REASONING_EFFORT".into(),
        "high".into(),
    )];
    opts.cli = vec![
        Override::set("roles.summary.reasoning_effort=max").unwrap(),
        Override::set("roles.summary=[\"opencode-messages/claude-haiku-5-5\"]").unwrap(),
    ];
    let cli = EffectiveConfig::load(opts).unwrap();
    assert_eq!(
        cli.settings().unwrap().roles.summary.reasoning_effort,
        ReasoningEffort::Max
    );
    assert_eq!(
        cli.settings().unwrap().roles.chapter.reasoning_effort,
        ReasoningEffort::Low
    );
    let rendered: Value = toml::from_str(&cli.render().unwrap()).unwrap();
    assert_eq!(
        rendered["roles"]["summary"]["reasoning_effort"].as_str(),
        Some("max")
    );
    for value in ["low", "medium", "high", "xhigh", "max"] {
        let mut opts = options(root.path());
        opts.cli = vec![Override::set(&format!("roles.decider.reasoning_effort={value}")).unwrap()];
        assert!(EffectiveConfig::load(opts).is_ok());
    }
    for entry in [
        "roles.summary.reasoning_effort=none",
        "roles.decider.reasoning_effort=disabled",
        "roles.chapter.reasoning_effort=minimal",
        "providers.codex.reasoning.reasoning_effort=none",
        "providers.opencode.reasoning.thinking={type='disabled'}",
    ] {
        let mut opts = options(root.path());
        opts.cli = vec![Override::set(entry).unwrap()];
        assert!(EffectiveConfig::load(opts).is_err(), "{entry}");
    }
}

#[test]
fn messages_provider_config_resolves_roles_and_custom_endpoints() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("roles.summary=[\"opencode-messages/claude-haiku-5-5\"]").unwrap(),
        Override::set("providers.opencode-messages.base_url='http://127.0.0.1:12345/v1'").unwrap(),
        Override::set("providers.opencode-messages.key_env='MESSAGES_FIXTURE_KEY'").unwrap(),
        Override::set("providers.anthropic={kind='anthropic-messages',base_url='https://api.anthropic.com/v1',key_env='ANTHROPIC_API_KEY'}").unwrap(),
    ];
    let config = EffectiveConfig::load(opts).unwrap();
    let settings = config.settings().unwrap();
    assert_eq!(
        settings.roles.summary.models,
        ["opencode-messages/claude-haiku-5-5"]
    );
    assert_eq!(settings.roles.chapter.models, settings.roles.summary.models);
    assert_eq!(
        settings.providers["anthropic"].kind.as_deref(),
        Some("anthropic-messages")
    );
    assert_eq!(
        settings.endpoints.opencode_messages_base_url.as_deref(),
        Some("http://127.0.0.1:12345/v1")
    );
    assert_eq!(
        settings.endpoints.opencode_messages_key_env,
        "MESSAGES_FIXTURE_KEY"
    );
    assert!(config.render().unwrap().contains("anthropic-messages"));

    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("providers.opencode-messages.reasoning={thinking={type='disabled'}}")
            .unwrap(),
    ];
    assert!(EffectiveConfig::load(opts).is_err());
}

#[test]
fn no_config_defaults_equal_pinned_model_roles_and_endpoints() {
    let root = tempfile::tempdir().unwrap();
    let settings = EffectiveConfig::load(options(root.path()))
        .unwrap()
        .settings()
        .unwrap();
    assert_eq!(settings.main_model, "gpt-6-luna");
    assert_eq!(settings.main_provider, "codex");
    let expected = ModelRoles::for_models(
        "opencode/deepseek-v4.1-flash".into(),
        "codex/gpt-6-luna".into(),
    );
    for (actual, expected) in [
        (&settings.roles.summary, &expected.summary),
        (&settings.roles.chapter, &expected.chapter),
        (&settings.roles.decider, &expected.decider),
        (&settings.roles.approver, &expected.approver),
    ] {
        assert_eq!(actual.models, expected.models);
        assert_eq!(actual.repeats, expected.repeats);
        assert_eq!(actual.threshold, expected.threshold);
    }
    let expected = ProviderEndpoints::default();
    assert_eq!(
        settings.endpoints.opencode_base_url,
        expected.opencode_base_url
    );
    assert_eq!(
        settings.endpoints.opencode_key_env,
        expected.opencode_key_env
    );
    assert_eq!(
        settings.endpoints.opencode_reasoning_fields,
        expected.opencode_reasoning_fields
    );
    assert_eq!(
        settings.endpoints.opencode_messages_base_url,
        expected.opencode_messages_base_url
    );
    assert_eq!(
        settings.endpoints.opencode_messages_key_env,
        expected.opencode_messages_key_env
    );
    assert_eq!(settings.endpoints.openai_base_url, expected.openai_base_url);
    assert_eq!(settings.endpoints.openai_key_env, expected.openai_key_env);
    assert_eq!(
        settings.endpoints.openrouter_decisions_url,
        expected.openrouter_decisions_url
    );
    assert_eq!(
        settings.endpoints.openrouter_key_env,
        expected.openrouter_key_env
    );
    assert_eq!(settings.compact_threshold_tokens, 200_000);
    assert_eq!(settings.verbatim_user_tokens, 20_000);
    assert_eq!(settings.retry_base_ms, 500);
    assert_eq!(settings.approvals.confirm, "ask");
}
