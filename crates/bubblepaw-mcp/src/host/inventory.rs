//! Inventory bookkeeping over the host state: tool definitions, diffs and
//! the registered-tool table.
use super::*;

pub(super) fn definitions(state: &HostState) -> BTreeMap<String, Value> {
    state
        .tools
        .iter()
        .map(|(n, t)| (n.clone(), serde_json::to_value(&t.definition).unwrap()))
        .collect()
}
pub(super) fn inventory_diff(
    before: &BTreeMap<String, Value>,
    state: &mut HostState,
    diff: &mut McpDiff,
) {
    let after = definitions(state);
    diff.added = after
        .keys()
        .filter(|n| !before.contains_key(*n))
        .cloned()
        .collect();
    diff.removed = before
        .keys()
        .filter(|n| !after.contains_key(*n))
        .cloned()
        .collect();
    diff.changed = after
        .iter()
        .filter(|(n, d)| before.get(*n).is_some_and(|old| old != *d))
        .map(|(n, _)| n.clone())
        .collect();
    for name in &diff.removed {
        state.removed.entry(name.clone()).or_insert_with(|| {
            "tool removed from the current server list or configuration filter".into()
        });
    }
    for name in after.keys() {
        state.removed.remove(name);
    }
}
pub(super) fn mark_removed(state: &mut HostState, server_name: &str, reason: &str) {
    for (name, tool) in &state.tools {
        if tool.server_name == server_name {
            state
                .removed
                .insert(name.clone(), format!("server {server_name}: {reason}"));
        }
    }
}
pub(super) fn rebuild(state: &mut HostState, warnings: &mut Vec<String>) {
    let mut tools = BTreeMap::new();
    let mut registry = ToolRegistry::new();
    for (name, entry) in &state.servers {
        let server = &entry.server;
        for listed in &entry.listed {
            let Some(original_name) = listed["name"].as_str() else {
                warnings.push(format!("MCP server {name}: skipped tool without a name"));
                continue;
            };
            if !entry.config.allows_tool(original_name) {
                continue;
            }
            let registered = tool_name(name, original_name);
            if tools.contains_key(&registered) {
                warnings.push(format!(
                    "MCP tool {registered}: skipped duplicate registered name"
                ));
                continue;
            }
            let Some(schema) = listed.get("inputSchema") else {
                warnings.push(format!(
                    "MCP tool {registered}: skipped missing input schema"
                ));
                continue;
            };
            let connection = server.connection.lock().unwrap().clone();
            if connection
                .as_ref()
                .is_some_and(|c| c.register_tool(original_name, schema).is_err())
            {
                warnings.push(format!(
                    "MCP tool {registered}: skipped invalid HTTP header annotation"
                ));
                continue;
            }
            let approval = entry
                .config
                .tools
                .get(original_name)
                .and_then(|t| t.approval)
                .unwrap_or(entry.config.approval);
            let definition = ToolDefinition {
                name: registered.clone(),
                description: listed["description"].as_str().unwrap_or("").into(),
                input_schema: schema.clone(),
                read_only: false,
                approval,
            };
            // Compile before publishing; compiler errors may contain remote
            // contents, so only a fixed diagnostic escapes.
            if registry
                .register(Arc::new(McpTool {
                    host: Weak::new(),
                    definition: definition.clone(),
                }))
                .is_err()
            {
                warnings.push(format!(
                    "MCP tool {registered}: skipped invalid input schema"
                ));
                continue;
            }
            tools.insert(
                registered,
                Registered {
                    server_name: name.clone(),
                    server: server.clone(),
                    original_name: original_name.into(),
                    definition,
                },
            );
        }
    }
    state.tools = tools;
}
