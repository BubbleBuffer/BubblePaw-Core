//! Execution-mode defaults, layers and removed-key diagnostics.
use super::*;

fn options(root: &Path) -> LoadOptions {
    LoadOptions {
        workspace: root.join("workspace"),
        home: Some(root.join("home")),
        custom_file: None,
        env: vec![],
        cli: vec![],
    }
}
fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
fn override_value(path: &str, value: &str) -> Override {
    Override::value(path, parse_value(value))
}

#[test]
fn execution_mode_layers_and_require_restart() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    let defaults = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(
        defaults.settings().unwrap().agent.execution_mode,
        ExecutionMode::Native
    );
    write(
        &opts.workspace.join(".bubblepaw/config.toml"),
        "[agent]\nexecution_mode='js'\n",
    );
    let file = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(
        file.settings().unwrap().agent.execution_mode,
        ExecutionMode::Js
    );
    assert!(file.restart_needed(&defaults));
    opts.env = vec![("BUBBLEPAW_AGENT__EXECUTION_MODE".into(), "'native'".into())];
    let environment = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(
        environment.settings().unwrap().agent.execution_mode,
        ExecutionMode::Native
    );
    opts.cli = vec![override_value("agent.execution_mode", "'js'")];
    let cli = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(
        cli.settings().unwrap().agent.execution_mode,
        ExecutionMode::Js
    );
    assert_eq!(cli.source("agent.execution_mode"), Some("CLI"));
    let rendered: Value = cli.render().unwrap().parse().unwrap();
    assert_eq!(rendered["agent"]["execution_mode"].as_str(), Some("js"));
    opts.cli = vec![override_value("agent.execution_mode", "'invalid'")];
    assert!(EffectiveConfig::load(opts).is_err());
}

#[test]
fn removed_enable_js_is_rejected_with_execution_mode_guidance() {
    for value in ["true", "false"] {
        let root = tempfile::tempdir().unwrap();
        let opts = options(root.path());
        write(
            &opts.workspace.join(".bubblepaw/config.toml"),
            &format!("[agent]\nenable_js={value}\n"),
        );
        let error = EffectiveConfig::load(opts.clone()).err().unwrap();
        assert!(
            error.to_string().contains("execution_mode = \"js\""),
            "{error}"
        );
        std::fs::remove_file(opts.workspace.join(".bubblepaw/config.toml")).unwrap();
        let mut environment = opts.clone();
        environment.env = vec![("BUBBLEPAW_AGENT__ENABLE_JS".into(), value.into())];
        let error = EffectiveConfig::load(environment).err().unwrap();
        assert!(
            error.to_string().contains("execution_mode = \"js\""),
            "{error}"
        );
        let mut cli = opts;
        cli.cli = vec![override_value("agent.enable_js", value)];
        let error = EffectiveConfig::load(cli).err().unwrap();
        assert!(
            error.to_string().contains("execution_mode = \"js\""),
            "{error}"
        );
    }
}
