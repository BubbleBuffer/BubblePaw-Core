//! The section schema is the one key list: docs, liveness and projection read it.
use super::*;
use bubblepaw_core::config_schema::{descend, leaves};

fn options(root: &Path) -> LoadOptions {
    LoadOptions {
        workspace: root.join("workspace"),
        home: Some(root.join("home")),
        custom_file: None,
        env: vec![],
        cli: vec![],
    }
}

fn schema_leaves() -> Vec<(String, bool)> {
    let mut all = Vec::new();
    leaves(Node::Table(RawSettings::FIELDS), "", &mut all);
    all
}

fn is_fixed(node: Node, key: &str) -> bool {
    match node {
        Node::Open(fields, _) => fields.iter().any(|field| field.name == key),
        _ => false,
    }
}

/// Key paths the config docs write, with map names as `<name>`; `None` marks a key the schema lacks.
fn documented(node: Node, path: &str, value: &Value, out: &mut Vec<String>) {
    let table = value.as_table().filter(|table| !table.is_empty());
    let Some(table) =
        table.filter(|_| matches!(node, Node::Table(_) | Node::Map(_) | Node::Open(..)))
    else {
        return out.push(path.to_owned());
    };
    for (key, value) in table {
        let name = if matches!(node, Node::Map(_))
            || (matches!(node, Node::Open(..)) && !is_fixed(node, key))
        {
            "<name>"
        } else {
            key
        };
        let child = if path.is_empty() {
            name.to_owned()
        } else {
            format!("{path}.{name}")
        };
        match descend(node, key) {
            Some(inner) => documented(inner, &child, value, out),
            None => out.push(format!("unknown:{child}")),
        }
    }
}

fn docs_paths() -> Vec<String> {
    let text = include_str!("../../../docs/config.md").replace("\r\n", "\n");
    let mut paths = Vec::new();
    for block in text.split("```toml\n").skip(1) {
        let block = block.split("```").next().unwrap();
        // Commented-out `# key = value` lines document optional keys.
        let source: Vec<String> = block
            .lines()
            .map(|line| match line.strip_prefix("# ") {
                Some(rest)
                    if rest.split_once('=').is_some_and(|(key, _)| {
                        !key.trim().is_empty()
                            && key
                                .trim()
                                .chars()
                                .all(|c| c.is_ascii_lowercase() || c == '_')
                    }) =>
                {
                    rest.to_owned()
                }
                _ => line.to_owned(),
            })
            .collect();
        let value: Value = toml::from_str(&source.join("\n"))
            .unwrap_or_else(|e| panic!("docs toml block: {e}\n{block}"));
        documented(Node::Table(RawSettings::FIELDS), "", &value, &mut paths);
    }
    paths
}

#[test]
fn every_documented_key_is_a_schema_field_and_every_field_is_documented() {
    let schema = schema_leaves();
    let mut seen = std::collections::BTreeSet::new();
    for (path, _) in &schema {
        assert!(seen.insert(path), "{path} is declared twice");
    }
    let docs = docs_paths();
    let unknown: Vec<_> = docs
        .iter()
        .filter(|path| path.starts_with("unknown:"))
        .collect();
    assert!(
        unknown.is_empty(),
        "documented keys missing from the schema: {unknown:?}"
    );
    let missing: Vec<_> = schema
        .iter()
        .map(|(path, _)| path)
        .filter(|leaf| {
            !docs.iter().any(|doc| {
                leaf == &doc
                    || leaf.starts_with(&format!("{doc}."))
                    || doc.starts_with(&format!("{leaf}."))
            })
        })
        .collect();
    assert!(
        missing.is_empty(),
        "schema keys missing from docs/config.md: {missing:?}"
    );
}

#[test]
fn known_paths_come_from_the_schema() {
    for path in [
        "model.main",
        "providers.x.base_url",
        "providers.x.reasoning.thinking.type",
        "context.lcm.index_tokens",
        "context.lcm.episode_prompt",
        "mcp.servers.s.headers_from.X-Api.Key",
        "mcp.servers.s.tools.t.approval",
        "roles.summary.models",
        "web.expose.fetch",
    ] {
        assert!(known_path(path), "{path}");
    }
    for path in [
        "model.mian",
        "providers..kind",
        "storage.data_dir.x",
        "roles.other",
        "context.lcm.nope",
    ] {
        assert!(!known_path(path), "{path}");
    }
}

#[test]
fn liveness_is_declared_per_key() {
    let live: BTreeMap<_, _> = schema_leaves().into_iter().collect();
    for (path, expected) in [
        ("model.main", true),
        ("context.window_tokens", true),
        ("context.strategy", false),
        ("context.lcm.index_tokens", false),
        ("web.cache_ttl_secs", false),
        ("web.expose.fetch", true),
        ("storage.data_dir", false),
        ("mcp.servers.<name>.url", true),
        ("agent.max_rounds", false),
    ] {
        assert_eq!(live[path], expected, "{path}");
    }
}

#[test]
fn live_projection_takes_live_keys_and_keeps_startup_only_ones() {
    let root = tempfile::tempdir().unwrap();
    let startup = EffectiveConfig::load(options(root.path())).unwrap();
    let mut opts = options(root.path());
    opts.cli = [
        "context.window_tokens=64000",
        "context.strategy=classical",
        "web.cache_ttl_secs=5",
        "web.cooldown_secs=7",
        "providers.extra={kind='scripted'}",
    ]
    .map(|entry| Override::set(entry).unwrap())
    .to_vec();
    let edited = EffectiveConfig::load(opts).unwrap();
    let changes = edited.changes_from(&startup);
    assert!(
        changes
            .iter()
            .any(|c| c.path == "context.strategy" && !c.live)
    );
    assert!(
        changes
            .iter()
            .any(|c| c.path == "web.cooldown_secs" && c.live)
    );
    let projected = startup.with_live_from(&edited).settings().unwrap();
    assert_eq!(projected.context_window_tokens, Some(64000));
    assert_eq!(projected.web.cooldown_secs, 7);
    assert_eq!(
        projected.web.cache_ttl_secs,
        startup.settings().unwrap().web.cache_ttl_secs
    );
    assert!(matches!(projected.context_strategy, ContextStrategy::Lcm));
    assert!(projected.providers.contains_key("extra"));
    assert!(projected.configured_providers.contains("extra"));
}
