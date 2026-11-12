//! Attach keeps layers and only the referenced environment.
use super::*;

#[test]
fn attach_config_keeps_layers_and_only_referenced_environment() {
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path());
    options.env = vec![
        ("CODEX_HOME".into(), "credentials".into()),
        ("OPENAI_API_KEY".into(), "private-client-credential".into()),
        ("BP_FAKE_STEP_LOG".into(), "client-log".into()),
        ("ATTACH_MCP_TOKEN".into(), "private-mcp-credential".into()),
        ("CRAWL4AI_API_TOKEN".into(), "client-crawl-token".into()),
        ("BUBBLEPAW_STORAGE__DATA_DIR".into(), "records".into()),
    ];
    options
        .cli
        .push(Override::set("skills.dirs=['skills']").unwrap());
    options.cli.push(
        Override::set("web.fetch_backend=['direct','crawl4ai:http://127.0.0.1:11235']").unwrap(),
    );
    options
        .cli
        .push(Override::set("mcp.servers.fixture.command='fixture'").unwrap());
    options
        .cli
        .push(Override::set("mcp.servers.fixture.env_from.TOKEN='ATTACH_MCP_TOKEN'").unwrap());
    // A provider credential that an MCP server also references.
    options
        .cli
        .push(Override::set("mcp.servers.fixture.env_from.SHARED='OPENAI_API_KEY'").unwrap());
    let mut original = EffectiveConfig::load(options).unwrap();
    original.anchor_paths(root.path());
    // Even an accidental serialization before preparation cannot send the
    // full local snapshot. The explicit transport snapshot is allowlisted.
    assert!(
        !serde_json::to_string(&original)
            .unwrap()
            .contains("client-log")
    );
    original.prepare_for_attach();
    let wire = serde_json::to_string(&original).unwrap();
    assert!(!wire.contains("client-log"));
    assert!(!wire.contains("CODEX_HOME"));
    let snapshot: EffectiveConfig =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    for config in [snapshot.clone(), snapshot.reload().unwrap()] {
        let settings = config.settings().unwrap();
        assert_eq!(settings.storage.data_dir, root.path().join("records"));
        assert_eq!(settings.skills.dirs, vec![root.path().join("skills")]);
        assert_eq!(config.environment("CODEX_HOME"), None);
        assert_eq!(
            config.environment("CRAWL4AI_API_TOKEN").as_deref(),
            Some("client-crawl-token")
        );
        assert_eq!(
            settings.providers["codex"].auth_file,
            Some(root.path().join("credentials/auth.json"))
        );
        assert_eq!(
            config.codex_models_cache(),
            Some(root.path().join("credentials/models_cache.json"))
        );
        assert_eq!(config.environment("UNSET_CLIENT_CREDENTIAL"), None);
        assert_eq!(
            config.environment("OPENAI_API_KEY").as_deref(),
            Some("private-client-credential")
        );
        assert_eq!(config.environment("BP_FAKE_STEP_LOG").as_deref(), None);
        assert_eq!(
            config.mcp_environment(),
            vec![
                ("ATTACH_MCP_TOKEN".into(), "private-mcp-credential".into()),
                ("OPENAI_API_KEY".into(), "private-client-credential".into()),
            ]
        );
        let runtime = config.runtime_environment();
        assert!(
            !runtime
                .iter()
                .any(|(_, value)| value == "private-mcp-credential")
        );
        // Also referenced by a provider, so subprocesses see it as standalone ones would.
        assert!(runtime.iter().any(|(name, value)| {
            name == "OPENAI_API_KEY" && value == "private-client-credential"
        }));
        assert!(!format!("{config:?}").contains("private-client-credential"));
        assert!(
            !config
                .render()
                .unwrap()
                .contains("private-client-credential")
        );
        assert!(
            !config
                .load_options()
                .env
                .iter()
                .any(|(name, _)| name == "OPENAI_API_KEY")
        );
    }
}

#[test]
fn only_opencode_aliases_are_opencode_routes() {
    use super::is_opencode_route;
    assert!(is_opencode_route("opencode"));
    assert!(is_opencode_route("opencode-messages"));
    assert!(!is_opencode_route("api"));
    assert!(!is_opencode_route("anthropic"));
    assert!(!is_opencode_route("openai"));
    assert!(!is_opencode_route("codex"));
}
