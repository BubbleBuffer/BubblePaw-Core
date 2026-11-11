//! MCP server settings, environment references and tool filters.
use super::*;

#[test]
fn mcp_env_from_names_are_exempt_from_overrides_and_never_render_values() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mcp.toml");
    write(
        &path,
        "[mcp.servers.github]\ncommand = \"github-mcp-server\"\nargs = [\"stdio\"]\n\
         env = { LOG_LEVEL = \"warn\" }\n\
         env_from = { GITHUB_PERSONAL_ACCESS_TOKEN = \"BUBBLEPAW_SYNTHETIC_GITHUB_TOKEN\" }\n",
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(path);
    // The host variable named by env_from is a credential name, so it is not a config override.
    opts.env = vec![(
        "BUBBLEPAW_SYNTHETIC_GITHUB_TOKEN".into(),
        "never-render-this-value".into(),
    )];
    let effective = EffectiveConfig::load(opts).unwrap();
    let settings = effective.settings().unwrap();
    let github = &settings.mcp.servers["github"];
    assert_eq!(
        github.env_from["GITHUB_PERSONAL_ACCESS_TOKEN"],
        "BUBBLEPAW_SYNTHETIC_GITHUB_TOKEN"
    );
    assert_eq!(github.env["LOG_LEVEL"], "warn");
    let rendered = effective.render().unwrap();
    assert!(rendered.contains("BUBBLEPAW_SYNTHETIC_GITHUB_TOKEN"));
    assert!(!rendered.contains("never-render-this-value"));
}

#[test]
fn mcp_config_rejects_unknown_keys_and_invalid_values() {
    let root = tempfile::tempdir().unwrap();
    for entry in [
        "mcp.servers.github.bogus=true",
        "mcp.servers.github.defer=\"yes\"",
        "mcp.servers.github.approval=\"auto\"",
        "mcp.servers.github.tools.get_issue.approval=\"auto\"",
        "mcp.servers.github.startup_timeout_secs=0",
        "mcp.servers.github.call_timeout_secs=\"soon\"",
        "mcp.servers.github.env_from={ TOKEN = \"A=B\" }",
        "mcp.servers.github.env_from={ TOKEN = \"\" }",
        "mcp.servers.github.env_from={ \"A=B\" = \"TOKEN\" }",
        "mcp.servers.github.env={ \"\" = \"x\" }",
        "mcp.servers.gi.thub.command=\"github\"",
    ] {
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("mcp.servers.github.command=\"github-mcp-server\"").unwrap(),
            Override::set(entry).unwrap(),
        ];
        assert!(EffectiveConfig::load(opts).is_err(), "{entry}");
    }
    let mut opts = options(root.path());
    opts.cli = vec![Override::set("mcp.servers.github.command=\"\"").unwrap()];
    assert!(EffectiveConfig::load(opts).is_err());
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("mcp.servers.github.command=\"github-mcp-server\"").unwrap(),
        Override::set("mcp.servers.github.approval=\"none\"").unwrap(),
    ];
    assert!(EffectiveConfig::load(opts).is_ok());
}

#[test]
fn mcp_http_layers_allowlist_and_secret_name_exemptions() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.env = vec![
        (
            "BUBBLEPAW_MCP__SERVERS__REMOTE__URL".into(),
            "https://example.com/mcp".into(),
        ),
        (
            "BUBBLEPAW_SYNTHETIC_AUTH".into(),
            "Bearer never-render-this-value".into(),
        ),
    ];
    opts.cli = vec![
        Override::set("mcp.header_key_envs=['BUBBLEPAW_SYNTHETIC_AUTH']").unwrap(),
        Override::set("mcp.servers.remote.headers_from={Authorization='BUBBLEPAW_SYNTHETIC_AUTH'}")
            .unwrap(),
    ];
    let effective = EffectiveConfig::load(opts).unwrap();
    let settings = effective.settings().unwrap();
    assert_eq!(
        settings.mcp.header_key_envs,
        vec!["BUBBLEPAW_SYNTHETIC_AUTH"]
    );
    assert_eq!(
        settings.mcp.servers["remote"].url.as_deref(),
        Some("https://example.com/mcp")
    );
    assert_eq!(
        settings.mcp.servers["remote"].headers_from["Authorization"],
        "BUBBLEPAW_SYNTHETIC_AUTH"
    );
    assert_eq!(effective.source("mcp.servers.remote.url"), Some("env"));
    let rendered = effective.render().unwrap();
    assert!(rendered.contains("BUBBLEPAW_SYNTHETIC_AUTH"));
    assert!(!rendered.contains("never-render-this-value"));
    assert!(!format!("{:?}", settings.mcp).contains("never-render-this-value"));
}

#[test]
fn mcp_http_invalid_types_and_values_are_rejected_without_echoing_values() {
    let root = tempfile::tempdir().unwrap();
    for entry in [
        "mcp.header_key_envs='synthetic-secret'",
        "mcp.header_key_envs=['Bearer synthetic-secret']",
        "mcp.servers.remote.url=42",
        "mcp.servers.remote.url='http://example.com/synthetic-secret'",
        "mcp.servers.remote.url='https://user:synthetic-secret@example.com/mcp'",
        "mcp.servers.remote.url='https://example.com/mcp?token=synthetic-secret'",
        "mcp.servers.remote.url='https://example.com/mcp#synthetic-secret'",
        "mcp.servers.remote.command='synthetic-secret'",
        "mcp.servers.remote.args=['synthetic-secret']",
        "mcp.servers.remote.cwd='synthetic-secret'",
        "mcp.servers.remote.env={LOG_LEVEL='synthetic-secret'}",
        "mcp.servers.remote.env_from={TOKEN='SYNTHETIC_TOKEN'}",
        "mcp.servers.remote.headers_from='synthetic-secret'",
        "mcp.servers.remote.headers_from={Authorization=42}",
        "mcp.servers.remote.headers_from={Authorization='Bearer synthetic-secret'}",
        "mcp.servers.remote.headers_from={Authorization='DISALLOWED_AUTH'}",
        "mcp.servers.remote.headers_from={Authorization='MCP_TOKEN',authorization='MCP_TOKEN'}",
        "mcp.servers.remote.headers_from={Accept='MCP_TOKEN'}",
        "mcp.servers.remote.headers_from={'Mcp-Session-Id'='MCP_TOKEN'}",
        "mcp.servers.remote.headers_from={'bad header'='MCP_TOKEN'}",
    ] {
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("mcp.servers.remote.url='https://example.com/mcp'").unwrap(),
            Override::set(entry).unwrap(),
        ];
        let error = format!("{:#}", EffectiveConfig::load(opts).expect_err(entry));
        assert!(!error.contains("synthetic-secret"), "{error}");
    }
}

#[test]
fn mcp_http_header_env_override_is_a_reference() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.env = vec![
        (
            "BUBBLEPAW_MCP__SERVERS__REMOTE__URL".into(),
            "http://127.0.0.2/mcp".into(),
        ),
        (
            "BUBBLEPAW_MCP__SERVERS__REMOTE__HEADERS_FROM__AUTHORIZATION".into(),
            "BUBBLEPAW_SYNTHETIC_MCP_TOKEN".into(),
        ),
        (
            "BUBBLEPAW_SYNTHETIC_MCP_TOKEN".into(),
            "Bearer never-render-this-value".into(),
        ),
    ];
    let effective = EffectiveConfig::load(opts).unwrap();
    assert_eq!(
        effective.settings().unwrap().mcp.servers["remote"].headers_from["authorization"],
        "BUBBLEPAW_SYNTHETIC_MCP_TOKEN"
    );
    assert!(
        !effective
            .render()
            .unwrap()
            .contains("never-render-this-value")
    );
}

#[test]
fn mcp_http_dotted_header_file_preserves_secret_reference() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mcp.toml");
    write(
        &path,
        "[mcp.servers.remote]\nurl = 'https://example.com/mcp'\nheaders_from = {'X.Trace' = 'BUBBLEPAW_CUSTOM_TOKEN'}\n",
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(path);
    opts.env = vec![(
        "BUBBLEPAW_CUSTOM_TOKEN".into(),
        "never-render-this-value".into(),
    )];
    let effective = EffectiveConfig::load(opts).unwrap();
    assert_eq!(
        effective.settings().unwrap().mcp.servers["remote"].headers_from["X.Trace"],
        "BUBBLEPAW_CUSTOM_TOKEN"
    );
    let rendered = effective.render().unwrap();
    assert!(rendered.contains("BUBBLEPAW_CUSTOM_TOKEN"));
    assert!(!rendered.contains("never-render-this-value"));
    let reloaded: Value = toml::from_str(&rendered).unwrap();
    assert_eq!(
        reloaded["mcp"]["servers"]["remote"]["headers_from"]["X.Trace"].as_str(),
        Some("BUBBLEPAW_CUSTOM_TOKEN")
    );
}

#[test]
fn mcp_tool_filters_layer_render_and_preserve_explicit_empty() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("mcp.servers.fixture.command='fixture'").unwrap(),
        Override::set("mcp.servers.fixture.include_tools=['read_*']").unwrap(),
        Override::set("mcp.servers.fixture.exclude_tools=['*_secret']").unwrap(),
    ];
    let effective = EffectiveConfig::load(opts.clone()).unwrap();
    let settings = effective.settings().unwrap();
    assert_eq!(
        settings.mcp.servers["fixture"].include_tools,
        Some(vec!["read_*".into()])
    );
    assert_eq!(settings.mcp.servers["fixture"].exclude_tools, ["*_secret"]);
    assert!(effective.render().unwrap().contains("include_tools"));
    opts.cli
        .push(Override::set("mcp.servers.fixture.include_tools=[]").unwrap());
    let empty = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(
        empty.settings().unwrap().mcp.servers["fixture"].include_tools,
        Some(Vec::new())
    );
    let rendered: Value = toml::from_str(&empty.render().unwrap()).unwrap();
    assert!(
        rendered["mcp"]["servers"]["fixture"]["include_tools"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for invalid in ["'read_*'", "[1]", "true"] {
        opts.cli
            .push(Override::set(&format!("mcp.servers.fixture.include_tools={invalid}")).unwrap());
        assert!(EffectiveConfig::load(opts.clone()).is_err());
    }
}
