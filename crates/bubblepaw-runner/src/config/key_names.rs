//! Names of host secrets that config values refer to (`*.key_env`, MCP `env_from`).
use super::join_path;
use std::collections::BTreeSet;
use toml::Value;

pub(super) fn key_names(value: &Value) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    collect_key_names(value, "", &mut names);
    names
}
pub(super) fn collect_key_names(value: &Value, path: &str, names: &mut BTreeSet<String>) {
    if (path.ends_with(".key_env") || path == "web.deepseek_key_env")
        && let Some(name) = value.as_str()
    {
        names.insert(name.into());
    }
    // An env_from value names a host secret; it is exempt from generic overrides like key_env.
    let parts: Vec<_> = path.split('.').collect();
    if matches!(
        parts.as_slice(),
        ["mcp", "servers", _, "env_from", _] | ["mcp", "servers", _, "headers_from", _, ..]
    ) && let Some(name) = value.as_str()
    {
        names.insert(name.into());
    }
    if let Value::Table(table) = value {
        for (key, value) in table {
            collect_key_names(value, &join_path(path, key), names);
        }
    }
}
