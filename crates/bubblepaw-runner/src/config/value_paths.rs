//! Value path helpers: parsing, key validation, merging and rendering leaves.
use super::*;

pub fn parse_value(value: &str) -> Value {
    toml::from_str::<Value>(&format!("value = {value}"))
        .ok()
        .and_then(|mut table| {
            let table = table.as_table_mut()?;
            (table.len() == 1).then(|| table.remove("value"))?
        })
        .unwrap_or_else(|| Value::String(value.into()))
}
pub(super) fn validate_keys(value: &Value, path: &str) -> Result<()> {
    ensure!(
        path != "agent.enable_js",
        "agent.enable_js was removed; use execution_mode = \"js\" in [agent] to select JavaScript mode"
    );
    if !path.is_empty() {
        ensure!(known_path(path), "Unknown config key: {path}");
    }
    if let Value::Table(table) = value {
        // Dots are valid HTTP token characters, not nested configuration keys.
        let header_table = matches!(
            path.split('.').collect::<Vec<_>>().as_slice(),
            ["mcp", "servers", _, "headers_from"]
        );
        for (key, value) in table {
            ensure!(
                !key.is_empty() && (header_table || !key.contains('.')),
                "Unknown config key: {}",
                join_path(path, key)
            );
            validate_keys(value, &join_path(path, key))?;
        }
    }
    Ok(())
}
pub(super) fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.into()
    } else {
        format!("{path}.{key}")
    }
}
pub(super) fn merge_at(target: &mut Value, parts: &[&str], value: Value) -> Result<()> {
    if let Some((part, rest)) = parts.split_first() {
        if !target.is_table() {
            *target = Value::Table(toml::map::Map::new());
        }
        let table = target.as_table_mut().unwrap();
        let entry = table
            .entry((*part).to_owned())
            .or_insert_with(|| Value::Table(toml::map::Map::new()));
        merge_at(entry, rest, value)
    } else {
        match (target, value) {
            (Value::Table(target), Value::Table(value)) if !value.is_empty() => {
                for (key, value) in value {
                    merge_at(
                        target
                            .entry(key)
                            .or_insert_with(|| Value::Table(toml::map::Map::new())),
                        &[],
                        value,
                    )?;
                }
            }
            (target, value) => *target = value,
        }
        Ok(())
    }
}
pub(super) fn normalize_roles(value: &mut Value, path: &str) {
    if matches!(
        path,
        "roles.summary" | "roles.chapter" | "roles.decider" | "roles.approver"
    ) && !value.is_table()
    {
        let models = if value.is_array() {
            value.clone()
        } else {
            Value::Array(vec![value.clone()])
        };
        let mut table = toml::map::Map::new();
        table.insert("models".into(), models);
        *value = Value::Table(table);
    } else if let Value::Table(table) = value {
        for (key, value) in table {
            normalize_roles(value, &join_path(path, key));
        }
    }
}
pub(super) fn collect_sources(
    value: &Value,
    path: &str,
    source: &str,
    sources: &mut BTreeMap<String, String>,
) {
    if let Value::Table(table) = value
        && !table.is_empty()
    {
        for (key, value) in table {
            collect_sources(value, &join_path(path, key), source, sources);
        }
    } else {
        sources.insert(path.into(), source.into());
    }
}
pub(super) fn record_override(
    value: &Value,
    path: &str,
    source: &str,
    base: Option<&Path>,
    sources: &mut BTreeMap<String, String>,
    bases: &mut BTreeMap<String, PathBuf>,
) {
    if let Value::Table(table) = value
        && !table.is_empty()
    {
        sources.remove(path);
        for (key, value) in table {
            record_override(value, &join_path(path, key), source, base, sources, bases);
        }
    } else {
        sources.retain(|key, _| key != path && !key.starts_with(&format!("{path}.")));
        bases.retain(|key, _| key != path && !key.starts_with(&format!("{path}.")));
        sources.insert(path.into(), source.into());
        if let Some(base) = base {
            bases.insert(path.into(), base.to_path_buf());
        }
    }
}
pub(super) fn render_leaves(
    value: &Value,
    parts: &mut Vec<String>,
    sources: &BTreeMap<String, String>,
    output: &mut String,
) {
    if let Value::Table(table) = value
        && !table.is_empty()
    {
        for (key, value) in table {
            parts.push(key.clone());
            render_leaves(value, parts, sources, output);
            parts.pop();
        }
    } else {
        let path = parts.join(".");
        let source = sources
            .get(&path)
            .map(String::as_str)
            .unwrap_or("default")
            .replace(['\r', '\n'], " ");
        output.push_str(&format!(
            "# source: {source}\n{} = {value}\n",
            parts
                .iter()
                .map(|part| Value::String(part.clone()).to_string())
                .collect::<Vec<_>>()
                .join(".")
        ));
    }
}
/// Whether a configured provider alias is an OpenCode Go route. Only these
/// get the `x-opencode-session` header. The alias decides, not the URL: the
/// same wire kind serves OpenCode and plain provider hosts alike.
pub fn is_opencode_route(name: &str) -> bool {
    name == "opencode" || name.starts_with("opencode-")
}
