use super::*;

const CONFIG: &str = "\
[model]
provider='demo'
main='demo/first'
[providers.demo]
kind='scripted'
models=[{id='first'}]
";

fn options(root: &Path, config: &str) -> ConfigOptions {
    std::fs::write(root.join("config.toml"), config).unwrap();
    ConfigOptions {
        workspace: Some(root.to_owned()),
        data_dir: Some(root.join("data")),
        config_file: Some(root.join("config.toml")),
        home: None,
        env: Some(Vec::new()),
        overrides: Vec::new(),
    }
}

#[test]
fn loads_the_cli_host_request() {
    let root = tempfile::tempdir().unwrap();
    let config = HostConfig::load(options(root.path(), CONFIG)).unwrap();
    assert_eq!(
        config.data_dir(),
        std::path::absolute(root.path().join("data")).unwrap()
    );
    assert_eq!(config.workspace(), root.path().canonicalize().unwrap());
    assert!(!config.is_central());
    assert_eq!(config.settings().main_model, "demo/first");
    let request = config.into_request();
    assert!(matches!(request.cli.command, crate::cli::Command::Stdio));
    assert_eq!(
        request.cli.data_dir.as_deref(),
        Some(request.data_dir.as_path())
    );
}

#[test]
fn a_set_override_follows_the_data_dir_option() {
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path(), CONFIG);
    let other = root.path().join("other");
    options.overrides = vec![Override::set(&format!("storage.data_dir={:?}", other)).unwrap()];
    let config = HostConfig::load(options).unwrap();
    assert_eq!(config.data_dir(), std::path::absolute(other).unwrap());
}

#[test]
fn a_missing_config_file_names_its_path() {
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path(), CONFIG);
    let missing = root.path().join("missing.toml");
    options.config_file = Some(missing.clone());
    match HostConfig::load(options) {
        Err(ConfigError::File { path, message }) => {
            assert_eq!(path, missing);
            assert!(
                message.starts_with("cannot read custom config"),
                "{message}"
            );
        }
        other => panic!("expected a file error, got {:?}", other.err()),
    }
}

#[test]
fn an_invalid_project_file_names_its_path() {
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path(), CONFIG);
    options.config_file = None;
    std::fs::create_dir(root.path().join(".bubblepaw")).unwrap();
    let project = root.path().join(".bubblepaw/config.toml");
    std::fs::write(&project, "[model\nkey='secret'").unwrap();
    match HostConfig::load(options) {
        Err(ConfigError::File { path, message }) => {
            assert_eq!(path, project);
            assert!(!message.contains("secret"), "{message}");
        }
        other => panic!("expected a file error, got {:?}", other.err()),
    }
}

#[test]
fn an_unresolved_role_is_a_role_error() {
    let root = tempfile::tempdir().unwrap();
    let config = format!("{CONFIG}[roles]\nsummary=['unqualified']\n");
    match HostConfig::load(options(root.path(), &config)) {
        Err(ConfigError::Roles(message)) => assert!(message.contains("roles.summary"), "{message}"),
        other => panic!("expected a role error, got {:?}", other.err()),
    }
}

#[test]
fn an_unknown_provider_is_a_provider_error() {
    let root = tempfile::tempdir().unwrap();
    let config = CONFIG.replace("provider='demo'", "provider='absent'");
    let config = config.replace("main='demo/first'", "main='absent/first'");
    match HostConfig::load(options(root.path(), &config)) {
        Err(ConfigError::Providers(_)) => {}
        other => panic!("expected a provider error, got {:?}", other.err()),
    }
}

#[test]
fn a_missing_workspace_is_a_workspace_error() {
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path(), CONFIG);
    options.workspace = Some(root.path().join("absent"));
    assert!(matches!(
        HostConfig::load(options),
        Err(ConfigError::Workspace { .. })
    ));
}

#[test]
fn loads_on_a_first_run_without_any_provider_or_credential() {
    let root = tempfile::tempdir().unwrap();
    let config = HostConfig::load(ConfigOptions {
        workspace: Some(root.path().to_owned()),
        data_dir: Some(root.path().join("data")),
        config_file: None,
        home: Some(root.path().join("home")),
        env: Some(Vec::new()),
        overrides: Vec::new(),
    })
    .unwrap();
    assert!(!config.settings().main_model.is_empty());
    assert!(!config.data_dir().join("host.json").exists());
}

#[test]
fn a_configured_provider_whose_key_is_unset_still_loads() {
    let root = tempfile::tempdir().unwrap();
    let config = "\
[model]
provider='keyed'
main='keyed/model'
[providers.keyed]
kind='chat-completions'
base_url='http://127.0.0.1:9/v1'
key_env='KEYED_API_KEY'
models=[{id='model'}]
";
    let loaded = HostConfig::load(options(root.path(), config)).unwrap();
    assert_eq!(loaded.settings().main_model, "keyed/model");
}
