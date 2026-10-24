//! Shared discovery for native model tools and optional execution adapters.
use crate::{Tool, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

/// Search an already policy-filtered tool snapshot. `select:name,other` loads
/// exact schemas; ordinary queries rank name, description and MCP server words.
/// The caller owns policy: this function never broadens the supplied snapshot.
#[cfg(test)]
pub fn search_tool_definitions(
    definitions: &[ToolDefinition],
    query: &str,
    limit: Option<usize>,
) -> Value {
    search_tool_definitions_with_servers(definitions, query, limit, |_| None)
}

/// Search with the registered, original MCP server identity. Tool names may
/// contain a sanitized or truncated server label, so discovery never infers
/// identity from a function name.
pub fn search_tool_definitions_with_servers<'a>(
    definitions: &[ToolDefinition],
    query: &str,
    limit: Option<usize>,
    tool_server: impl Fn(&str) -> Option<&'a str>,
) -> Value {
    if let Some(names) = query.trim().strip_prefix("select:") {
        let mut tools = Vec::new();
        let mut unknown = Vec::new();
        for name in names
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            match definitions.iter().find(|tool| tool.name == name) {
                Some(tool) => tools.push(tool_json(tool, tool_server(&tool.name))),
                None => unknown.push(name),
            }
        }
        return json!({"tools": tools, "unknown": unknown});
    }
    let terms = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    let mut ranked = definitions
        .iter()
        .map(|tool| {
            let name = tool.name.to_lowercase();
            let description = tool.description.to_lowercase();
            let server = tool_server(&tool.name).unwrap_or("").to_lowercase();
            let score = terms
                .iter()
                .map(|term| {
                    let name_score = if name == *term {
                        20
                    } else if name.contains(term.as_str()) {
                        10
                    } else {
                        0
                    };
                    name_score
                        + if description.contains(term.as_str()) {
                            2
                        } else {
                            0
                        }
                        + if server.contains(term.as_str()) { 4 } else { 0 }
                })
                .sum::<usize>();
            (score, tool)
        })
        .filter(|(score, _)| *score > 0)
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    json!({"tools": ranked.into_iter().take(limit.unwrap_or(5).clamp(1, 50))
        .map(|(_, tool)| tool_json(tool, tool_server(&tool.name))).collect::<Vec<_>>()})
}

fn tool_json(tool: &ToolDefinition, server: Option<&str>) -> Value {
    let mut result =
        json!({"name":tool.name,"description":tool.description,"inputSchema":tool.input_schema});
    if let Some(server) = server {
        result["server"] = json!(server);
    }
    result
}

/// The first sentence of a description, suitable for a compact tool catalog.
pub fn tool_definition_summary(description: &str) -> String {
    let line = description.lines().next().unwrap_or("").trim();
    match line.find(". ") {
        Some(end) => line[..=end].to_owned(),
        None => line.to_owned(),
    }
}

pub(crate) fn register_discovery_tools(registry: &mut ToolRegistry) -> Result<()> {
    for tool in [Arc::new(ToolSearch) as Arc<dyn Tool>, Arc::new(CallTool)] {
        if registry.get(&tool.definition().name).is_none() {
            registry.register(tool)?;
        }
    }
    Ok(())
}

struct ToolSearch;
#[async_trait]
impl Tool for ToolSearch {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tool_search".into(),
            description: "Find available tools by name, description or MCP server. Returns names, descriptions and argument schemas. Use select:name,other to load exact tools, then invoke a discovered tool with call_tool.".into(),
            input_schema: json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":50}},"required":["query"],"additionalProperties":false}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        bail!("tool_search requires the host tool dispatcher")
    }
    async fn invoke_with_dispatcher(
        &self,
        _: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        let query = input["query"]
            .as_str()
            .context("tool_search requires query")?;
        Ok(ToolResult::text(
            search_tool_definitions_with_servers(
                &dispatcher.definitions(),
                query,
                input["limit"].as_u64().map(|limit| limit as usize),
                |name| dispatcher.tool_server(name),
            )
            .to_string(),
        ))
    }
}

struct CallTool;
#[async_trait]
impl Tool for CallTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "call_tool".into(),
            description: "Invoke an available tool by name with its JSON arguments. Call declared tools directly; use call_tool for tools found with tool_search. The target keeps its normal permissions, approval and execution limits.".into(),
            input_schema: json!({"type":"object","properties":{"name":{"type":"string"},"arguments":{"type":"object"}},"required":["name","arguments"],"additionalProperties":false}),
            // The wrapper itself has no effect. The dispatcher classifies and
            // approves the actual target, including in read-only runs.
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        bail!("call_tool requires the host tool dispatcher")
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        let name = input["name"].as_str().context("call_tool requires name")?;
        dispatcher
            .invoke(context, name, input["arguments"].clone())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            read_only: true,
            approval: Default::default(),
        }
    }

    #[test]
    fn discovery_matches_name_description_and_mcp_server_with_full_schemas() {
        let definitions = vec![
            definition("read", "Read a workspace file."),
            definition("mcp__calendar__events", "Find upcoming appointments."),
            definition("search", "Search workspace files."),
        ];
        for (query, expected) in [
            ("read", "read"),
            ("appointments", "mcp__calendar__events"),
            ("calendar", "mcp__calendar__events"),
        ] {
            let result = search_tool_definitions(&definitions, query, None);
            assert_eq!(result["tools"][0]["name"], expected);
            assert_eq!(
                result["tools"][0]["inputSchema"],
                definitions
                    .iter()
                    .find(|tool| tool.name == expected)
                    .unwrap()
                    .input_schema
            );
        }
        let selected = search_tool_definitions(&definitions, "select:read,hidden", None);
        assert_eq!(selected["tools"].as_array().unwrap().len(), 1);
        assert_eq!(selected["unknown"], json!(["hidden"]));
    }

    #[test]
    fn discovery_uses_only_supplied_snapshot_and_breaks_ties_by_name() {
        let definitions = vec![
            definition("zebra", "Read files."),
            definition("alpha", "Read files."),
        ];
        let result = search_tool_definitions(&definitions, "files", Some(1));
        assert_eq!(result["tools"][0]["name"], "alpha");
        assert!(
            search_tool_definitions(&definitions, "hidden", None)["tools"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            tool_definition_summary("Read files. More detail.\nOther line"),
            "Read files."
        );
    }

    #[test]
    fn discovery_matches_original_server_identity_without_guessing_from_name() {
        let definitions = vec![definition("mcp__short__events", "Upcoming appointments.")];
        let server = "Family calendar / Europe (original server identity)";
        let found =
            search_tool_definitions_with_servers(&definitions, "Europe", None, |_| Some(server));
        assert_eq!(found["tools"][0]["server"], server);
        assert_eq!(found["tools"][0]["name"], "mcp__short__events");
        assert!(
            search_tool_definitions(&definitions, "Europe", None)["tools"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
