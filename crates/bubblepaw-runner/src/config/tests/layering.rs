//! Layer precedence, explicit config, environment and set overrides, unknown keys.
use super::*;

#[test]
fn layer_precedence_default_user_project_env_cli() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    assert_eq!(
        EffectiveConfig::load(options(root.path()))
            .unwrap()
            .settings()
            .unwrap()
            .compact_threshold_tokens,
        200_000
    );
    write(
        &opts.home.as_ref().unwrap().join(".bubblepaw/config.toml"),
        "[context]\ncompact_threshold_tokens=100\n",
    );
    assert_eq!(
        EffectiveConfig::load(options(root.path()))
            .unwrap()
            .settings()
            .unwrap()
            .compact_threshold_tokens,
        100
    );
    write(
        &opts.workspace.join(".bubblepaw/config.toml"),
        "[context]\ncompact_threshold_tokens=200\n",
    );
    assert_eq!(
        EffectiveConfig::load(options(root.path()))
            .unwrap()
            .settings()
            .unwrap()
            .compact_threshold_tokens,
        200
    );
    opts.env.push((
        "BUBBLEPAW_CONTEXT__COMPACT_THRESHOLD_TOKENS".into(),
        "300".into(),
    ));
    assert_eq!(
        EffectiveConfig::load(LoadOptions {
            cli: vec![],
            ..options_with_env(root.path(), opts.env.clone())
        })
        .unwrap()
        .settings()
        .unwrap()
        .compact_threshold_tokens,
        300
    );
    opts.cli
        .push(override_value("context.compact_threshold_tokens", "400"));
    let config = EffectiveConfig::load(opts).unwrap();
    assert_eq!(config.settings().unwrap().compact_threshold_tokens, 400);
    assert_eq!(
        config.source("context.compact_threshold_tokens"),
        Some("CLI")
    );
}
pub(super) fn options_with_env(root: &Path, env: Vec<(String, String)>) -> LoadOptions {
    LoadOptions {
        env,
        ..options(root)
    }
}

#[test]
fn explicit_config_replaces_user_and_project_and_requires_file() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    write(
        &opts.home.as_ref().unwrap().join(".bubblepaw/config.toml"),
        "[model]\nmain='user'\n",
    );
    write(
        &opts.workspace.join(".bubblepaw/config.toml"),
        "[context]\nstrategy='classical'\n",
    );
    let custom = root.path().join("custom.toml");
    write(&custom, "[model]\nmain='custom'\n");
    opts.custom_file = Some(custom.clone());
    let config = EffectiveConfig::load(opts).unwrap();
    assert_eq!(config.settings().unwrap().main_model, "custom");
    assert!(matches!(
        config.settings().unwrap().context_strategy,
        ContextStrategy::Lcm
    ));
    let config = EffectiveConfig::load(options_with_env(
        root.path(),
        vec![(
            "BUBBLEPAW_CONFIG".into(),
            custom.to_string_lossy().into_owned(),
        )],
    ))
    .unwrap();
    assert_eq!(config.settings().unwrap().main_model, "custom");
    let mut opts = options(root.path());
    opts.custom_file = Some(root.path().join("missing.toml"));
    assert!(EffectiveConfig::load(opts).is_err());
    let chosen = root.path().join("chosen.toml");
    write(&chosen, "[model]\nmain='chosen'\n");
    let mut opts = options_with_env(
        root.path(),
        vec![(
            "BUBBLEPAW_CONFIG".into(),
            custom.to_string_lossy().into_owned(),
        )],
    );
    opts.custom_file = Some(chosen);
    assert_eq!(
        EffectiveConfig::load(opts)
            .unwrap()
            .settings()
            .unwrap()
            .main_model,
        "chosen"
    );
}

#[test]
fn env_double_underscore_maps_nested_paths() {
    let root = tempfile::tempdir().unwrap();
    let config = EffectiveConfig::load(options_with_env(
        root.path(),
        vec![
            ("BUBBLEPAW_CONTEXT__STRATEGY".into(), "classical".into()),
            (
                "BUBBLEPAW_CONTEXT__CLASSICAL__VERBATIM_USER_TOKENS".into(),
                "1000".into(),
            ),
            (
                "BUBBLEPAW_PROVIDERS__OPENCODE__REASONING__REASONING_EFFORT".into(),
                "medium".into(),
            ),
            ("BUBBLEPAW_APPROVALS__TOOLS__BASH".into(), "allow".into()),
        ],
    ))
    .unwrap();
    let settings = config.settings().unwrap();
    assert!(matches!(
        settings.context_strategy,
        ContextStrategy::Classical
    ));
    assert_eq!(settings.verbatim_user_tokens, 1000);
    assert_eq!(settings.approvals.tools["bash"], "allow");
    assert_eq!(
        settings.endpoints.opencode_reasoning_fields["reasoning_effort"],
        "medium"
    );
}

#[test]
fn set_parses_toml_values_and_falls_back_to_strings() {
    assert_eq!(
        Override::set("model.main=provider/my-model").unwrap().value,
        Value::String("provider/my-model".into())
    );
    assert_eq!(
        Override::set("context.compact_threshold_tokens=1234")
            .unwrap()
            .value,
        Value::Integer(1234)
    );
    assert_eq!(
        Override::set("context.auto_summarize=false").unwrap().value,
        Value::Boolean(false)
    );
    assert!(
        Override::set("roles.summary=['main']")
            .unwrap()
            .value
            .is_array()
    );
    assert!(
        Override::set("roles.summary={models=['main'], repeats=2}")
            .unwrap()
            .value
            .is_table()
    );
    assert_eq!(
        parse_value("'first'\nsecond='value'"),
        Value::String("'first'\nsecond='value'".into())
    );
    assert!(Override::set("missing-equals").is_err());
}

#[test]
fn unknown_file_key_names_exact_path() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path());
    write(
        &opts.workspace.join(".bubblepaw/config.toml"),
        "[context]\nmisspelled=1\n",
    );
    assert!(
        EffectiveConfig::load(opts)
            .unwrap_err()
            .to_string()
            .contains("context.misspelled")
    );
}
#[test]
fn unknown_env_key_names_exact_path() {
    let root = tempfile::tempdir().unwrap();
    let error = EffectiveConfig::load(options_with_env(
        root.path(),
        vec![("BUBBLEPAW_CONTEXT__MISSPELLED".into(), "1".into())],
    ))
    .unwrap_err();
    assert!(error.to_string().contains("context.misspelled"));
}
#[test]
fn unknown_set_key_names_exact_path() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli
        .push(Override::set("context.misspelled=1").unwrap());
    assert!(
        EffectiveConfig::load(opts)
            .unwrap_err()
            .to_string()
            .contains("context.misspelled")
    );
}
