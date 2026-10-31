use super::*;

#[test]
fn tools_defer_by_default_and_core_policy_needs_no_new_transport() {
    let deferred: McpServerSettings =
        serde_json::from_value(serde_json::json!({"command": "fixture"})).unwrap();
    assert!(deferred.defer);
    assert!(McpServerSettings::default().defer);
    let core: McpServerSettings = serde_json::from_value(serde_json::json!({
        "command": "fixture", "defer": false
    }))
    .unwrap();
    assert!(!core.defer);
    assert!(deferred.same_connection_settings(&core));
    assert_ne!(deferred, core);
    let encoded = serde_json::to_value(core).unwrap();
    assert_eq!(encoded["defer"], false);
}

#[test]
fn tool_filters_distinguish_omitted_and_empty_include() {
    let mut settings: McpServerSettings = serde_json::from_value(serde_json::json!({
        "command": "fixture", "include_tools": ["read_*"], "exclude_tools": ["*_secret"]
    }))
    .unwrap();
    assert!(settings.allows_tool("read_note"));
    assert!(!settings.allows_tool("read_secret"));
    assert!(!settings.allows_tool("echo"));
    settings.include_tools = Some(vec!["echo".into(), "read_*".into()]);
    settings.exclude_tools = vec!["read_note".into(), "*_secret".into()];
    assert!(settings.allows_tool("echo"));
    assert!(!settings.allows_tool("read_note"));
    assert!(!settings.allows_tool("read_secret"));
    settings.include_tools = Some(Vec::new());
    assert!(!settings.allows_tool("echo"));
    let encoded = serde_json::to_value(&settings).unwrap();
    assert_eq!(encoded["include_tools"], serde_json::json!([]));
    let decoded: McpServerSettings = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.include_tools, Some(Vec::new()));
    settings.include_tools = None;
    settings.exclude_tools.clear();
    assert!(settings.allows_tool("anything"));
    assert!(
        serde_json::to_value(&settings)
            .unwrap()
            .get("include_tools")
            .is_none()
    );
}

#[test]
fn tool_globs_match_raw_whole_names_with_only_star_special() {
    for (pattern, name, expected) in [
        ("read", "read", true),
        ("read", "read_note", false),
        ("read_*", "read_note", true),
        ("*note", "read_note", true),
        ("*read*note*", "my_read_a_note_v2", true),
        ("a**b*c", "axbybc", true),
        ("*", "猫の道具", true),
        ("猫*具", "猫の道具", true),
        ("a.*", "a.b", true),
        ("a.*", "a_b", false),
        ("read?", "readx", false),
        ("READ*", "read_note", false),
        ("*read", "read_note", false),
        ("", "", true),
        ("*", "", true),
    ] {
        assert_eq!(
            tool_pattern_matches(pattern, name),
            expected,
            "{pattern}: {name}"
        );
    }
}

fn http(url: &str) -> McpServerSettings {
    McpServerSettings {
        url: Some(url.into()),
        ..Default::default()
    }
}

#[test]
fn http_url_rules_match_provider_setup() {
    for url in [
        "https://example.com/mcp",
        "http://localhost/mcp",
        "http://dev.localhost:9000/mcp",
        "http://127.12.34.56/mcp",
        "http://[::1]/mcp",
    ] {
        assert!(http(url).validate(&[]).is_ok(), "{url}");
    }
    for url in [
        "http://example.com/mcp",
        "http://192.168.1.1/mcp",
        "http://localhost.example.com/mcp",
        "http://[::2]/mcp",
        "ftp://localhost/mcp",
        "https://user:synthetic-secret@example.com/mcp",
        "https://example.com/mcp?key=synthetic-secret",
        "https://example.com/mcp#synthetic-secret",
        "https://example.com/\nsynthetic-secret",
        "synthetic-secret",
    ] {
        let error = http(url).validate(&[]).unwrap_err().to_string();
        assert!(!error.contains("synthetic-secret"));
    }
}

#[test]
fn transports_are_exclusive_and_stdio_defaults_remain_valid() {
    assert!(McpServerSettings::default().validate(&[]).is_err());
    let stdio = McpServerSettings {
        command: "server".into(),
        ..Default::default()
    };
    assert!(stdio.validate(&[]).is_ok());
    let mut config = http("https://example.com/mcp");
    config.command = "server".into();
    assert!(config.validate(&[]).is_err());
    config.command.clear();
    config.args.push("arg".into());
    assert!(config.validate(&[]).is_err());
    config.args.clear();
    config.cwd = Some(".".into());
    assert!(config.validate(&[]).is_err());
    config.cwd = None;
    config.env.insert("LOG_LEVEL".into(), "warn".into());
    assert!(config.validate(&[]).is_err());
    config.env.clear();
    config.env_from.insert("TOKEN".into(), "HOST_TOKEN".into());
    assert!(config.validate(&[]).is_err());
    let mut stdio = stdio;
    stdio
        .headers_from
        .insert("authorization".into(), "HOST_TOKEN".into());
    assert!(stdio.validate(&[]).is_err());
}

#[test]
fn http_headers_require_env_names_and_reserve_protocol_headers() {
    let mut config = http("https://example.com/mcp");
    for name in ["MCP_API_KEY", "MCP_KEY", "MCP_TOKEN"] {
        config
            .headers_from
            .insert("Authorization".into(), name.into());
        assert!(config.validate(&[]).is_ok());
    }
    for name in [
        "CUSTOM_AUTH",
        "Bearer synthetic-secret",
        "1_BAD_TOKEN",
        "BAD-TOKEN",
        "",
        "BAD\n_TOKEN",
    ] {
        config
            .headers_from
            .insert("Authorization".into(), name.into());
        let error = config.validate(&[]).unwrap_err().to_string();
        assert!(!error.contains("synthetic-secret"));
    }
    config
        .headers_from
        .insert("Authorization".into(), "CUSTOM_AUTH".into());
    assert!(config.validate(&["CUSTOM_AUTH".into()]).is_ok());
    assert!(config.validate(&["bad identifier".into()]).is_err());
    config.headers_from.clear();
    for header in [
        "Accept",
        "Content-Type",
        "Host",
        "Content-Length",
        "MCP-Protocol-Version",
        "Mcp-Session-Id",
        "Last-Event-ID",
        "Mcp-Method",
        "Mcp-Name",
        "Mcp-Param-Test",
        "Connection",
        "Transfer-Encoding",
        "bad header",
        "X-\r\nsecret",
    ] {
        config
            .headers_from
            .insert(header.into(), "MCP_TOKEN".into());
        assert!(config.validate(&[]).is_err(), "{header}");
        config.headers_from.clear();
    }
    config
        .headers_from
        .insert("Authorization".into(), "MCP_TOKEN".into());
    config
        .headers_from
        .insert("authorization".into(), "MCP_TOKEN".into());
    assert!(config.validate(&[]).is_err());
}

#[test]
fn debug_omits_invalid_secret_references_and_url() {
    let mut config = http("https://user:synthetic-secret@example.com/mcp");
    config
        .headers_from
        .insert("Authorization".into(), "Bearer synthetic-secret".into());
    config
        .env_from
        .insert("CHILD".into(), "Bearer synthetic-secret".into());
    assert!(!format!("{config:?}").contains("synthetic-secret"));
}
