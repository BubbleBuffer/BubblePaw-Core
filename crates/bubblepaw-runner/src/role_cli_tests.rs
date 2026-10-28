use super::*;

#[test]
fn login_provider_does_not_collide_with_global_provider() {
    for verb in ["login", "logout"] {
        let cli =
            Cli::try_parse_from(["bubblepaw", verb, "opencode", "--provider", "codex"]).unwrap();
        match cli.command {
            Command::Login { provider, .. } | Command::Logout { provider, .. } => {
                assert!(matches!(provider, LoginProvider::Opencode));
            }
            _ => panic!("expected {verb}"),
        }
    }
}

#[test]
fn run_selects_exactly_one_prompt_source_and_reads_utf8_verbatim() {
    let positional = Cli::try_parse_from(["bubblepaw", "run", "original syntax"]).unwrap();
    let Command::Run {
        prompt,
        prompt_file,
        ..
    } = positional.command
    else {
        panic!("expected run");
    };
    assert_eq!(
        run_prompt(prompt.as_deref(), prompt_file.as_deref()).unwrap(),
        "original syntax"
    );
    assert!(Cli::try_parse_from(["bubblepaw", "run"]).is_err());
    assert!(
        Cli::try_parse_from(["bubblepaw", "run", "task", "--prompt-file", "task.txt"]).is_err()
    );

    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("worker task.txt");
    let contents = "Inspect the \"quoted\" path.\nPreserve literal $value and `code` — 猫.\n";
    std::fs::write(&file, contents).unwrap();
    let cli = Cli::try_parse_from([
        std::ffi::OsStr::new("bubblepaw"),
        std::ffi::OsStr::new("run"),
        std::ffi::OsStr::new("--prompt-file"),
        file.as_os_str(),
    ])
    .unwrap();
    let Command::Run {
        prompt,
        prompt_file,
        ..
    } = cli.command
    else {
        panic!("expected run");
    };
    assert_eq!(
        run_prompt(prompt.as_deref(), prompt_file.as_deref()).unwrap(),
        contents
    );
    std::fs::write(&file, [0xff, 0xfe]).unwrap();
    assert!(
        run_prompt(None, Some(&file))
            .unwrap_err()
            .to_string()
            .contains("UTF-8 prompt file")
    );
    assert!(run_prompt(None, Some(&root.path().join("missing.txt"))).is_err());
}

#[test]
fn root_prompt_override_replaces_instructions_and_keeps_execution_metadata() {
    let root = tempfile::tempdir().unwrap();
    let defaults = root_system_prompt(root.path(), None, ExecutionMode::Js);
    assert!(defaults.contains("raw command shell="));
    assert!(!defaults.contains("workspace access="));
    assert!(defaults.contains(". You are a software engineering agent"));
    assert!(
        defaults.ends_with("A binary file reference does not mean you have seen its contents.")
    );
    let custom = root_system_prompt(
        root.path(),
        Some("Only inspect the assigned files."),
        ExecutionMode::Js,
    );
    assert!(custom.contains(&format!("workspace={}", root.path().display())));
    let shell = if cfg!(windows) { "cmd.exe" } else { "sh" };
    assert!(custom.ends_with(&format!(
        "raw command shell={shell}. Only inspect the assigned files."
    )));
    assert!(!custom.contains("You are a software engineering agent"));
    assert!(root_system_prompt(root.path(), Some(""), ExecutionMode::Js).ends_with(". "));
}

#[test]
fn native_root_prompt_uses_available_tools_and_preserves_custom_instructions() {
    let root = tempfile::tempdir().unwrap();
    let prompt = root_system_prompt(root.path(), None, ExecutionMode::Native);
    assert!(prompt.contains("using the available tools"));
    assert!(prompt.contains("multiple tool calls in one message"));
    assert!(!prompt.contains("js"));
    assert!(!prompt.contains("text(markdown)"));
    let custom = root_system_prompt(
        root.path(),
        Some("Custom instructions."),
        ExecutionMode::Native,
    );
    assert!(custom.ends_with(" Custom instructions."));
}

fn roles_for_args(arguments: &[&str]) -> Result<bubblepaw_runner::model_roles::ModelRoles> {
    let cli = Cli::try_parse_from(arguments)?;
    let arguments: Vec<_> = arguments.iter().map(|arg| (*arg).to_owned()).collect();
    let root = tempfile::tempdir()?;
    let config = EffectiveConfig::load(LoadOptions {
        workspace: root.path().into(),
        home: None,
        custom_file: None,
        env: vec![],
        cli: config_overrides(&cli, &arguments, "demo")?,
    })?;
    Ok(config.settings_for_main(Some("demo/gpt-6-luna"))?.roles)
}

#[cfg(feature = "bench")]
#[test]
fn context_strategy_flags_select_run_and_replay_and_default_new_sessions_to_lcm() {
    let default = Cli::try_parse_from(["bubblepaw", "run", "hello"]).unwrap();
    assert_eq!(
        default
            .context_strategy
            .map(ContextStrategy::from)
            .unwrap_or_default(),
        ContextStrategy::Lcm
    );
    for (name, strategy) in [
        ("lcm", ContextStrategy::Lcm),
        ("classical", ContextStrategy::Classical),
    ] {
        let run =
            Cli::try_parse_from(["bubblepaw", "run", "hello", "--context-strategy", name]).unwrap();
        assert_eq!(
            run.context_strategy.map(ContextStrategy::from),
            Some(strategy)
        );
        let replay = Cli::try_parse_from([
            "bubblepaw",
            "replay",
            "--chat",
            "fixture.json",
            "--out",
            "output",
            "--dry-run",
            "--context-strategy",
            name,
        ])
        .unwrap();
        assert_eq!(
            replay.context_strategy.map(ContextStrategy::from),
            Some(strategy)
        );
    }
    assert!(
        Cli::try_parse_from(["bubblepaw", "run", "hello", "--context-strategy", "unknown"])
            .is_err()
    );
}

#[cfg(feature = "bench")]
#[test]
fn removed_context_strategy_is_rejected_by_run_and_replay() {
    for args in [
        vec!["bubblepaw", "run", "hello", "--context-strategy", "minimal"],
        vec![
            "bubblepaw",
            "replay",
            "--chat",
            "fixture.json",
            "--out",
            "output",
            "--context-strategy",
            "minimal",
        ],
    ] {
        let error = Cli::try_parse_from(args)
            .err()
            .expect("Removed strategy must be rejected")
            .to_string();
        assert!(error.contains("invalid value 'minimal'"), "{error}");
        assert!(error.contains("lcm, classical"), "{error}");
    }
}

#[test]
fn auto_approve_optional_model_keeps_subcommand_and_aliases() {
    let bare = Cli::try_parse_from(["bubblepaw", "--auto-approve", "run", "hello"]).unwrap();
    assert_eq!(bare.auto_approve.as_deref(), Some(""));
    assert!(matches!(bare.command, Command::Run { .. }));
    let named = Cli::try_parse_from([
        "bubblepaw",
        "--auto-approve",
        "openai/gpt-6-luna",
        "--confirm-auto",
        "edit,exec",
        "--episode-summary-model",
        "custom/summary",
        "run",
        "hello",
    ])
    .unwrap();
    assert_eq!(named.auto_approve.as_deref(), Some("openai/gpt-6-luna"));
    assert_eq!(named.confirm_auto, ["edit", "exec"]);
    assert_eq!(
        named.episode_summary_model.as_deref(),
        Some("custom/summary")
    );
    assert!(
        Cli::try_parse_from([
            "bubblepaw",
            "--auto-approve",
            "--approve-all",
            "run",
            "hello"
        ])
        .is_err()
    );
}

#[test]
fn storage_commands_parse_offline_and_data_dir_conflicts_with_legacy_db() {
    for args in [
        vec!["bubblepaw", "--auto-approve", "catalog", "rebuild"],
        vec!["bubblepaw", "--auto-approve", "serving", "wipe"],
        vec!["bubblepaw", "--auto-approve", "export", "session-id"],
        vec!["bubblepaw", "--auto-approve", "delete", "session-id"],
    ] {
        let cli = Cli::try_parse_from(&args).unwrap();
        assert_eq!(cli.auto_approve.as_deref(), Some(""));
        let arguments = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        config_overrides(&cli, &arguments, "demo").unwrap();
    }
    assert!(
        Cli::try_parse_from([
            "bubblepaw",
            "--db",
            "old.sqlite",
            "--data-dir",
            "state",
            "sessions"
        ])
        .is_err()
    );
}

#[test]
fn role_flags_keep_fallbacks_and_calibrate_luna_splits() {
    let arguments = [
        "bubblepaw",
        "--decider",
        "openai/gpt-6-luna",
        "--approver",
        "llm/demo/check",
        "--summary-model",
        "custom/summary",
        "run",
        "hello",
    ];
    let roles = roles_for_args(&arguments).unwrap();
    assert_eq!(roles.summary.models, ["custom/summary", "demo/gpt-6-luna"]);
    assert_eq!(roles.chapter.models, roles.summary.models);
    assert_eq!(
        roles.decider.models,
        [
            "openai/gpt-6-luna",
            "llm/custom/summary",
            "llm/demo/gpt-6-luna"
        ]
    );
    assert_eq!(roles.decider.repeats, None);
    assert_eq!(roles.decider.threshold, None);
    assert_eq!(
        roles.decider.decision_settings("openai/gpt-6-luna"),
        (1, 0.6)
    );
    assert_eq!(
        roles.decider.decision_settings("llm/demo/gpt-6-luna"),
        (3, 0.8)
    );
    assert_eq!(roles.approver.models[0], "llm/demo/check");
    assert_eq!(roles.approver.repeats, None);
    assert_eq!(roles.approver.threshold, None);
}

#[test]
fn decision_role_flags_require_qualified_models() {
    for flag in ["--decider", "--approver", "--auto-approve"] {
        for model in ["typesafe", "llm/bare", "openai/", "/model"] {
            let error = roles_for_args(&["bubblepaw", flag, model, "run", "hello"]).unwrap_err();
            assert!(
                error.to_string().contains("role")
                    || error.to_string().contains("Role")
                    || error.to_string().contains("model")
            );
        }
    }
}

#[test]
fn changing_decider_leaves_the_approver_unconfigured() {
    let arguments = [
        "bubblepaw",
        "--decider",
        "openrouter/typesafe/jev-1.13",
        "run",
        "hello",
    ];
    let roles = roles_for_args(&arguments).unwrap();
    assert!(!roles.decider.models.is_empty());
    // The approver has no built-in chain, so --decider does not set it.
    assert!(roles.approver.models.is_empty());
}
