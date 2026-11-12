//! Root agent, LCM, approval and hardline settings.
use super::*;

#[test]
fn root_agent_settings_keep_defaults_and_follow_config_layers() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    let defaults = EffectiveConfig::load(opts.clone()).unwrap();
    let settings = defaults.settings().unwrap();
    assert_eq!(settings.agent.max_rounds, None);
    assert!(settings.agent.system_prompt.is_none());
    assert_eq!(settings.agent.execution_mode, ExecutionMode::Native);
    write(
        &opts.home.as_ref().unwrap().join(".bubblepaw/config.toml"),
        "[agent]\nsystem_prompt='user instructions'\nmax_rounds=40\n",
    );
    write(
        &opts.workspace.join(".bubblepaw/config.toml"),
        "[agent]\nsystem_prompt='project instructions'\nmax_rounds=48\n",
    );
    let project = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(project.settings().unwrap().agent.max_rounds, Some(48));
    assert_eq!(
        project.settings().unwrap().agent.system_prompt.as_deref(),
        Some("project instructions")
    );
    opts.env = vec![
        (
            "BUBBLEPAW_AGENT__SYSTEM_PROMPT".into(),
            "env instructions".into(),
        ),
        ("BUBBLEPAW_AGENT__MAX_ROUNDS".into(), "64".into()),
    ];
    let environment = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(environment.settings().unwrap().agent.max_rounds, Some(64));
    assert_eq!(
        environment
            .settings()
            .unwrap()
            .agent
            .system_prompt
            .as_deref(),
        Some("env instructions")
    );
    opts.cli = vec![
        override_value("agent.system_prompt", "''"),
        override_value("agent.max_rounds", "1"),
    ];
    let cli = EffectiveConfig::load(opts).unwrap();
    assert_eq!(cli.settings().unwrap().agent.max_rounds, Some(1));
    assert_eq!(
        cli.settings().unwrap().agent.system_prompt.as_deref(),
        Some("")
    );
    assert_eq!(cli.source("agent.max_rounds"), Some("CLI"));
    assert!(cli.restart_needed(&defaults));
    let rendered: Value = toml::from_str(&cli.render().unwrap()).unwrap();
    assert_eq!(rendered["agent"]["system_prompt"].as_str(), Some(""));
}

#[test]
fn root_agent_settings_reject_zero_rounds_and_unknown_keys() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli = vec![override_value("agent.max_rounds", "0")];
    let error = EffectiveConfig::load(opts).err().unwrap();
    assert!(
        error
            .to_string()
            .contains("agent.max_rounds must be positive")
    );
    let mut opts = options(root.path());
    opts.cli = vec![override_value("agent.round_limit", "4")];
    assert!(
        EffectiveConfig::load(opts)
            .err()
            .unwrap()
            .to_string()
            .contains("Unknown config key: agent.round_limit")
    );
}

#[test]
fn lcm_sizes_are_layered_typed_and_visible_without_round_defaults() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli = vec![
        override_value("context.lcm.summary_input", "episode"),
        override_value("context.lcm.chapter_rollover_tokens", "0"),
        override_value("context.lcm.source_target_tokens", "16000"),
        override_value("context.lcm.source_max_tokens", "28000"),
        override_value("context.lcm.summary_target_tokens", "1200"),
        override_value("context.lcm.decision_context_tokens", "3000"),
        override_value("context.lcm.tool_evidence_tokens", "6000"),
        override_value("context.lcm.raw_tail_tokens", "16000"),
        override_value("context.lcm.apply_target_tokens", "45000"),
        override_value("context.lcm.output_reserve_tokens", "24000"),
    ];
    let config = EffectiveConfig::load(opts.clone()).unwrap();
    let settings = config.settings().unwrap();
    assert_eq!(settings.agent.max_rounds, None);
    assert_eq!(
        settings.lcm.episodes.summary_input,
        crate::episodes::EpisodeSummaryInput::Episode
    );
    assert_eq!(settings.lcm.episodes.chapter_rollover_tokens, 0);
    assert_eq!(settings.lcm.episodes.source_target_tokens, 16000);
    assert_eq!(settings.lcm.episodes.source_max_tokens, 28000);
    assert_eq!(settings.lcm.episodes.summary_target_tokens, 1200);
    assert_eq!(settings.lcm.episodes.decision_context_tokens, 3000);
    assert_eq!(settings.lcm.episodes.tool_evidence_tokens, 6000);
    assert_eq!(settings.lcm.raw_tail_tokens, 16000);
    assert_eq!(settings.lcm.apply_target_tokens, Some(45000));
    assert_eq!(settings.lcm.output_reserve_tokens, 24000);
    let rendered: Value = toml::from_str(&config.render().unwrap()).unwrap();
    assert_eq!(
        rendered["context"]["lcm"]["summary_input"].as_str(),
        Some("episode")
    );
    assert_eq!(
        rendered["context"]["lcm"]["source_target_tokens"].as_integer(),
        Some(16000)
    );
    assert!(rendered["agent"].get("max_rounds").is_none());
    opts.cli
        .push(override_value("context.lcm.source_max_tokens", "1000"));
    assert!(EffectiveConfig::load(opts).is_err());
}

#[test]
fn approval_deny_layers_validation_and_reload_classification() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    let file = opts.workspace.join(".bubblepaw/config.toml");
    write(&file, "[approvals]\ndeny=['exec','mcp__server__erase']\n");
    let base = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(
        base.settings().unwrap().approvals.deny,
        ["exec", "mcp__server__erase"]
    );
    assert!(
        base.source("approvals.deny")
            .unwrap()
            .starts_with("project:")
    );
    opts.cli = vec![Override::set("approvals.deny=['edit']").unwrap()];
    let next = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(next.settings().unwrap().approvals.deny, ["edit"]);
    assert!(!base.same_provider_config(&next));
    assert!(!next.restart_needed(&base));
    for text in [
        "[approvals]\ndeny=['']\n",
        "[approvals]\ndeny=['   ']\n",
        "[approvals]\ndeny='exec'\n",
        "[approvals]\ndeny=[123]\n",
        "[approval]\ndeny=['exec']\n",
    ] {
        write(&file, text);
        let mut invalid = opts.clone();
        invalid.cli.clear();
        assert!(EffectiveConfig::load(invalid).is_err(), "{text}");
    }
    write(&file, "[approvals]\ndeny=['exec']\n");
    opts.cli.clear();
    assert_eq!(
        EffectiveConfig::load(opts)
            .unwrap()
            .settings()
            .unwrap()
            .approvals
            .deny,
        ["exec"]
    );
}

#[test]
fn hardline_floor_defaults_on_and_only_user_cli_and_env_layers_change_it() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    let project = opts.workspace.join(".bubblepaw/config.toml");
    // The user file also holds the workspace trust entry; keep it.
    let user = root.path().join("home/.bubblepaw/config.toml");
    let trusted = std::fs::read_to_string(&user).unwrap();
    let hardline = |opts: &LoadOptions| {
        let config = EffectiveConfig::load(opts.clone()).unwrap();
        let on = config.settings().unwrap().approvals.hardline;
        assert_eq!(
            on,
            crate::live_config::approval_policy(&config.settings().unwrap())
                .unwrap()
                .hardline
        );
        (on, config.source("approvals.hardline").map(str::to_owned))
    };
    assert!(hardline(&opts).0);
    // A trusted project config cannot lower the floor.
    write(
        &project,
        "[approvals]
hardline=false
",
    );
    assert!(hardline(&opts).0);
    // The user config can.
    write(
        &user,
        "[approvals]
hardline=false
",
    );
    let (on, source) = hardline(&opts);
    assert!(!on && source.is_some_and(|s| s.starts_with("user:")));
    // CLI and env outrank the user config.
    opts.cli = vec![Override::set("approvals.hardline=true").unwrap()];
    assert!(hardline(&opts).0);
    opts.cli.clear();
    opts.env = vec![(
        "BUBBLEPAW_APPROVALS__HARDLINE".to_owned(),
        "true".to_owned(),
    )];
    assert!(hardline(&opts).0);
    opts.env.clear();
    for text in [
        "[approvals]
hardline='no'
",
        "[approvals]
hardline=1
",
    ] {
        write(
            &user,
            &format!(
                "{trusted}
{text}"
            ),
        );
        assert!(EffectiveConfig::load(opts.clone()).is_err(), "{text}");
    }
}
