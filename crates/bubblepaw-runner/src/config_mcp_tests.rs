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

#[test]
fn mcp_defaults_are_empty_and_env_overrides_apply_per_server() {
    use bubblepaw_core::Approval;
    let root = tempfile::tempdir().unwrap();
    let defaults = EffectiveConfig::load(options(root.path())).unwrap();
    assert!(defaults.settings().unwrap().mcp.servers.is_empty());
    assert_eq!(defaults.source("mcp"), Some("default"));

    let mut opts = options(root.path());
    opts.env = vec![
        (
            "BUBBLEPAW_MCP__SERVERS__GITHUB__COMMAND".into(),
            "github-mcp-server".into(),
        ),
        (
            "BUBBLEPAW_MCP__SERVERS__GITHUB__ARGS".into(),
            "[\"stdio\"]".into(),
        ),
        (
            "BUBBLEPAW_MCP__SERVERS__GITHUB__CALL_TIMEOUT_SECS".into(),
            "60".into(),
        ),
        (
            "BUBBLEPAW_MCP__SERVERS__GITHUB__TOOLS__GET_ISSUE__APPROVAL".into(),
            "none".into(),
        ),
    ];
    let effective = EffectiveConfig::load(opts).unwrap();
    let settings = effective.settings().unwrap();
    let github = &settings.mcp.servers["github"];
    assert_eq!(github.command, "github-mcp-server");
    assert_eq!(github.args, vec!["stdio"]);
    assert_eq!(github.call_timeout_secs, 60);
    assert_eq!(github.startup_timeout_secs, 30);
    assert!(github.enabled);
    assert!(github.defer);
    assert_eq!(github.approval, Approval::Confirm);
    assert_eq!(github.tools["get_issue"].approval, Some(Approval::None));
    assert_eq!(effective.source("mcp.servers.github.command"), Some("env"));
}

#[test]
fn mcp_server_defer_accepts_toml_env_and_cli_overrides() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    write(
        &file,
        "[mcp.servers.fixture]\ncommand='fixture'\ndefer=false\n",
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(file);
    let from_file = EffectiveConfig::load(opts.clone()).unwrap();
    assert!(!from_file.settings().unwrap().mcp.servers["fixture"].defer);
    let rendered: Value = toml::from_str(&from_file.render().unwrap()).unwrap();
    assert_eq!(
        rendered["mcp"]["servers"]["fixture"]["defer"].as_bool(),
        Some(false)
    );
    opts.env.push((
        "BUBBLEPAW_MCP__SERVERS__FIXTURE__DEFER".into(),
        "true".into(),
    ));
    let from_env = EffectiveConfig::load(opts.clone()).unwrap();
    assert!(from_env.settings().unwrap().mcp.servers["fixture"].defer);
    assert_eq!(from_env.source("mcp.servers.fixture.defer"), Some("env"));
    opts.cli = vec![Override::set("mcp.servers.fixture.defer=false").unwrap()];
    let from_cli = EffectiveConfig::load(opts).unwrap();
    assert!(!from_cli.settings().unwrap().mcp.servers["fixture"].defer);
    assert_eq!(from_cli.source("mcp.servers.fixture.defer"), Some("CLI"));
}
