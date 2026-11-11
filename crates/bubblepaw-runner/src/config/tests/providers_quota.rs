//! Config output, extensions, quota, accounts and provider aliases.
use super::*;

#[test]
fn config_output_shows_sources_and_never_provider_secret_values() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options_with_env(
        root.path(),
        vec![
            (
                "BUBBLEPAW_PRIVATE_KEY".into(),
                "secret-never-print-this".into(),
            ),
            ("OPENAI_API_KEY".into(), "second-secret".into()),
        ],
    );
    opts.cli = vec![
        Override::set("providers.opencode.key_env='BUBBLEPAW_PRIVATE_KEY'").unwrap(),
        Override::set("context.strategy=classical").unwrap(),
    ];
    let config = EffectiveConfig::load(opts).unwrap();
    let rendered = config.render().unwrap();
    assert!(rendered.contains("# source: default"));
    assert!(rendered.contains("# source: CLI"));
    assert!(rendered.contains("BUBBLEPAW_PRIVATE_KEY"));
    assert!(!rendered.contains("secret-never-print-this"));
    assert!(!rendered.contains("second-secret"));
    let parsed: Value = toml::from_str(&rendered).unwrap();
    assert_eq!(parsed, config.value);
}

#[test]
fn extensions_table_equals_old_json_and_resolves_relative_cwd() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("extensions.json");
    write(
        &path,
        r#"[{"id":"fixture","command":"worker","args":["--test"],"cwd":"relative"}]"#,
    );
    let old = load_extensions(&path).unwrap();
    let mut opts = options(root.path());
    opts.cli.push(Override::extensions_json(&path).unwrap());
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    assert_eq!(
        serde_json::to_value(&settings.extensions).unwrap(),
        serde_json::to_value(&old).unwrap()
    );
    let custom = root.path().join("config.toml");
    write(
        &custom,
        "[extensions.fixture]\ncommand='worker'\nargs=['--test']\ncwd='relative'\n",
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(custom);
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    assert_eq!(
        serde_json::to_value(&settings.extensions).unwrap(),
        serde_json::to_value(&old).unwrap()
    );
}

#[test]
fn quota_defaults_and_provider_accounts_use_entry_names() {
    let root = tempfile::tempdir().unwrap();
    let config = EffectiveConfig::load(options(root.path())).unwrap();
    let settings = config.settings().unwrap();
    assert_eq!(settings.quota, QuotaSettings::default());
    assert_eq!(settings.quota.max_wait_seconds, 21600);
    assert_eq!(config.source("quota.max_wait_seconds"), Some("default"));
    for (route, account) in settings.provider_accounts() {
        assert_eq!(route, account);
    }
}

#[test]
fn provider_and_extension_aliases_can_share_an_account() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    write(
        &path,
        "[providers.codex]\naccount='personal'\n\
         [providers.opencode]\naccount='personal'\n\
         [extensions.claude]\ncommand='bun'\naccount='personal'\n\
         [extensions.other]\ncommand='worker'\n\
         [quota]\nmax_wait_seconds=0\n",
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(path);
    let config = EffectiveConfig::load(opts).unwrap();
    let settings = config.settings().unwrap();
    let accounts = settings.provider_accounts();
    assert_eq!(accounts["codex"], "personal");
    assert_eq!(accounts["opencode"], "personal");
    assert_eq!(accounts["claude"], "personal");
    assert_eq!(accounts["other"], "other");
    assert_eq!(settings.quota.max_wait_seconds, 0);
    let rendered: Value = toml::from_str(&config.render().unwrap()).unwrap();
    assert_eq!(
        rendered["extensions"]["claude"]["account"].as_str(),
        Some("personal")
    );
}

#[test]
fn extension_json_preserves_account_override() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("extensions.json");
    write(
        &path,
        r#"[{"id":"claude","command":"bun","account":"shared"}]"#,
    );
    let mut opts = options(root.path());
    opts.cli.push(Override::extensions_json(&path).unwrap());
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    assert_eq!(settings.provider_accounts()["claude"], "shared");
    assert_eq!(settings.extensions[0].account.as_deref(), Some("shared"));
}

#[test]
fn account_labels_and_quota_wait_are_validated_without_credentials() {
    let root = tempfile::tempdir().unwrap();
    for value in ["''", "' '", "' padded '", "\"bad\\nlabel\""] {
        for route in ["providers.codex", "extensions.fixture"] {
            let mut opts = options(root.path());
            if route.starts_with("extensions") {
                opts.cli
                    .push(Override::set("extensions.fixture.command=worker").unwrap());
            }
            opts.cli
                .push(Override::set(&format!("{route}.account={value}")).unwrap());
            assert!(EffectiveConfig::load(opts).is_err(), "{route}: {value}");
        }
    }
    let mut opts = options(root.path());
    opts.cli
        .push(Override::set("quota.max_wait_seconds=-1").unwrap());
    assert!(EffectiveConfig::load(opts).is_err());
    let mut opts = options(root.path());
    opts.cli.push(Override::value(
        "providers.codex.account",
        Value::String("a".repeat(257)),
    ));
    assert!(EffectiveConfig::load(opts).is_err());
}

#[test]
fn quota_wait_changes_need_restart_while_provider_accounts_are_live() {
    let root = tempfile::tempdir().unwrap();
    let startup = EffectiveConfig::load(options(root.path())).unwrap();
    let mut opts = options(root.path());
    opts.cli
        .push(Override::set("providers.codex.account=shared").unwrap());
    let live = EffectiveConfig::load(opts.clone()).unwrap();
    assert!(!live.restart_needed(&startup));
    opts.cli
        .push(Override::set("quota.max_wait_seconds=7").unwrap());
    let restart = EffectiveConfig::load(opts).unwrap();
    assert!(restart.restart_needed(&startup));
}

#[test]
fn extension_env_table_reaches_settings_and_render_redacts_credentials() {
    let root = tempfile::tempdir().unwrap();
    let custom = root.path().join("config.toml");
    write(
        &custom,
        "[extensions.claude-code-b]\ncommand='bun'\nargs=['run','src/main.ts']\n\
         [extensions.claude-code-b.env]\nCLAUDE_CONFIG_DIR='/home/user/.claude-b'\n\
         BUBBLEPAW_CC_SYSTEM_PROMPT='replace'\nANTHROPIC_API_KEY='sk-private'\n",
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(custom);
    let config = EffectiveConfig::load(opts).unwrap();
    let settings = config.settings().unwrap();
    let env = &settings.extensions[0].env;
    assert_eq!(env["CLAUDE_CONFIG_DIR"], "/home/user/.claude-b");
    assert_eq!(env["BUBBLEPAW_CC_SYSTEM_PROMPT"], "replace");
    let rendered = config.render().unwrap();
    assert!(rendered.contains("/home/user/.claude-b"), "{rendered}");
    assert!(!rendered.contains("sk-private"), "{rendered}");
}

#[test]
fn partial_role_overrides_preserve_models_after_shorthand() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    write(
        &opts.workspace.join(".bubblepaw/config.toml"),
        "[roles]\nsummary='main'\n",
    );
    opts.cli
        .push(Override::set("roles.summary.repeats=2").unwrap());
    let role = EffectiveConfig::load(opts)
        .unwrap()
        .settings()
        .unwrap()
        .roles
        .summary;
    assert_eq!(role.models, ["codex/gpt-6-luna"]);
    assert_eq!(role.repeats, Some(2));
}

#[test]
fn empty_implicit_file_keeps_defaults() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path());
    write(
        &opts.workspace.join(".bubblepaw/config.toml"),
        "# no overrides\n",
    );
    assert_eq!(
        EffectiveConfig::load(opts)
            .unwrap()
            .settings()
            .unwrap()
            .main_model,
        "gpt-6-luna"
    );
}

#[test]
fn quoted_env_key_name_exempts_prefixed_credentials() {
    let root = tempfile::tempdir().unwrap();
    let config = EffectiveConfig::load(options_with_env(
        root.path(),
        vec![
            (
                "BUBBLEPAW_PROVIDERS__OPENCODE__KEY_ENV".into(),
                "'BUBBLEPAW_PRIVATE_KEY'".into(),
            ),
            ("BUBBLEPAW_PRIVATE_KEY".into(), "secret-value".into()),
        ],
    ))
    .unwrap();
    assert_eq!(
        config.settings().unwrap().endpoints.opencode_key_env,
        "BUBBLEPAW_PRIVATE_KEY"
    );
    assert!(!config.render().unwrap().contains("secret-value"));
}

#[test]
fn malformed_set_path_and_provider_id_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    for assignment in [
        "context..strategy=lcm",
        "providers.invalid/name={kind='responses',base_url='http://localhost',key_env='KEY'}",
    ] {
        let mut opts = options(root.path());
        opts.cli.push(Override::set(assignment).unwrap());
        assert!(EffectiveConfig::load(opts).is_err());
    }
}

#[test]
fn disabled_default_summary_preserves_explicit_decision_models() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("providers.opencode.base_url=''").unwrap(),
        Override::set("roles.decider='llm/opencode/deepseek-v4.1-flash'").unwrap(),
        Override::set("roles.chapter='summary'").unwrap(),
    ];
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    assert_eq!(
        settings.roles.decider.models,
        ["llm/opencode/deepseek-v4.1-flash"]
    );
    assert_eq!(settings.roles.chapter.models, ["codex/gpt-6-luna"]);
}

#[test]
fn configured_scripted_alias_registers_without_enabling_default_demo() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli
        .push(Override::set("providers.fixture={kind='scripted'}").unwrap());
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    let mut registry = ProviderRegistry::new(
        "main",
        Arc::new(crate::provider::ScriptedProvider::new("fixture")),
    );
    settings.register_providers(&mut registry).unwrap();
    assert!(registry.get("fixture").is_some());
    assert!(registry.get("demo").is_none());
}

#[test]
fn quoted_dotted_file_key_is_not_a_nested_known_key() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path());
    write(
        &opts.workspace.join(".bubblepaw/config.toml"),
        "'context.strategy'='classical'\n",
    );
    assert!(
        EffectiveConfig::load(opts)
            .unwrap_err()
            .to_string()
            .contains("Unknown config key: context.strategy")
    );
}
