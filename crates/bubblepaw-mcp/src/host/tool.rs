//! The registered tool for one MCP server tool: calls go through the
//! server's current generation, and results become core tool results.
use super::*;
use async_trait::async_trait;
use bubblepaw_core::{ToolContext, ToolResult};

pub(super) struct McpTool {
    pub(super) host: Weak<HostInner>,
    pub(super) definition: ToolDefinition,
}
#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        let Some(host) = self.host.upgrade() else {
            return Ok(ToolResult::error("MCP host shut down; call was not sent"));
        };
        let (registered, _lease) = {
            let state = host.state.lock().unwrap();
            if host.closed.load(Ordering::SeqCst) {
                return Ok(ToolResult::error("MCP host shut down; call was not sent"));
            }
            let Some(registered) = state.tools.get(&self.definition.name) else {
                let reason = state
                    .removed
                    .get(&self.definition.name)
                    .map(String::as_str)
                    .unwrap_or("tool unavailable in the current inventory");
                return Ok(ToolResult::error(format!(
                    "MCP tool {} removed: {reason}; call was not sent",
                    self.definition.name
                )));
            };
            if serde_json::to_value(&registered.definition)?
                != serde_json::to_value(&self.definition)?
            {
                return Ok(ToolResult::error(format!(
                    "MCP tool {} definition changed; refresh the tool registry before calling; call was not sent",
                    self.definition.name
                )));
            }
            registered.server.active.fetch_add(1, Ordering::SeqCst);
            (registered.clone(), Lease(registered.server.clone()))
        };
        let connected = tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => bail!("Cancelled before MCP call; outcome unknown"),
            result = registered.server.connected() => result,
        };
        let connection = match connected {
            Ok(connection) => connection,
            Err(error) => {
                return Ok(ToolResult::error(format!(
                    "MCP server unavailable: {error}; call was not sent"
                )));
            }
        };
        // Reconnect discovery is published at the next host boundary. Reject
        // removed or changed contracts before then rather than dispatching with
        // stale validation, header annotations, or approval metadata.
        if let Some(listed) = registered.server.refreshed.lock().unwrap().as_ref() {
            let Some(current) = listed
                .iter()
                .find(|tool| tool["name"] == registered.original_name)
            else {
                return Ok(ToolResult::error(format!(
                    "MCP tool {} removed by server reconnect; call was not sent",
                    self.definition.name
                )));
            };
            if current.get("inputSchema") != Some(&registered.definition.input_schema)
                || current["description"].as_str().unwrap_or("")
                    != registered.definition.description
            {
                return Ok(ToolResult::error(format!(
                    "MCP tool {} definition changed after server reconnect; refresh the tool registry; call was not sent",
                    self.definition.name
                )));
            }
        }
        if connection
            .register_tool(
                &registered.original_name,
                &registered.definition.input_schema,
            )
            .is_err()
        {
            return Ok(ToolResult::error(
                "Invalid MCP HTTP header annotation; call was not sent",
            ));
        }
        let call = connection.call_tool(
            &registered.original_name,
            input,
            &registered.definition.input_schema,
        );
        let result = tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => bail!("Cancelled during MCP call; outcome unknown"),
            result = tokio::time::timeout(Duration::from_secs(registered.server.config.call_timeout_secs), call) => result,
        };
        match result {
            Err(_) => Ok(ToolResult::error(
                "MCP call timed out; outcome unknown. Do not replay this call.",
            )),
            Ok(Err(error)) => Ok(ToolResult::error(connection.redact(&error.to_string()))),
            Ok(Ok(result)) => Ok(convert_result(result)),
        }
    }
}

/// Stable ASCII tool names, with a deterministic suffix for truncated names.
pub fn tool_name(server: &str, tool: &str) -> String {
    let original = format!("mcp__{server}__{tool}");
    let mut name: String = original
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.len() > 64 {
        let hash = bubblepaw_core::fingerprint(original.as_bytes());
        name.truncate(55);
        name.push('_');
        name.push_str(&hash[..8]);
    }
    name
}

fn convert_result(result: Value) -> ToolResult {
    if let Some(kind) = result.get("resultType")
        && kind != "complete"
    {
        return ToolResult::error(
            "Unsupported MCP result type or input request; outcome unknown. Do not replay this call.",
        );
    }
    let Some(content) = result.get("content").and_then(Value::as_array) else {
        return ToolResult::error("Invalid MCP tool result: missing content array");
    };
    let mut blocks = Vec::new();
    let mut has_text = false;
    for block in content {
        if block["type"] == "text" {
            if let Some(text) = block["text"].as_str() {
                has_text = true;
                blocks.push(text.to_owned());
            } else {
                return ToolResult::error("Invalid MCP text content");
            }
        } else {
            let kind = match block["type"].as_str() {
                Some("image") => "image",
                Some("audio") => "audio",
                Some("resource") => "resource",
                Some("resource_link") => "resource link",
                _ => "unknown content",
            };
            let bytes = block["data"]
                .as_str()
                .map(decoded_size)
                .unwrap_or_else(|| block["resource"].to_string().len());
            blocks.push(format!("[MCP {kind}: {bytes} bytes omitted]"));
        }
    }
    if !has_text && let Some(structured) = result.get("structuredContent") {
        blocks.insert(0, structured.to_string());
    }
    ToolResult {
        content: blocks.join("\n"),
        is_error: result["isError"].as_bool().unwrap_or(false),
        pending: false,
    }
}
fn decoded_size(data: &str) -> usize {
    let padding = data
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count()
        .min(2);
    (data.len() * 3 / 4).saturating_sub(padding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::Approval;

    #[test]
    fn names_are_bounded_ascii_and_stable() {
        assert_eq!(tool_name("a.b", "read/issue"), "mcp__a_b__read_issue");
        let long = "x".repeat(120);
        let name = tool_name("github", &long);
        assert_eq!(name.len(), 64);
        assert_eq!(name, tool_name("github", &long));
        assert_ne!(name, tool_name("github", &(long + "y")));
        assert_eq!(tool_name("é", "🐾"), "mcp______");
    }
    #[test]
    fn defaults_require_approval_and_debug_omits_values() {
        let mut config: McpServerSettings =
            serde_json::from_value(json!({"command":"test"})).unwrap();
        assert_eq!(config.approval, Approval::Confirm);
        assert_eq!(config.startup_timeout_secs, 30);
        assert_eq!(config.call_timeout_secs, 120);
        config
            .env
            .insert("NON_SECRET".into(), "should-not-print".into());
        config.env_from.insert("CHILD".into(), "HOST_NAME".into());
        let debug = format!("{config:?}");
        assert!(!debug.contains("should-not-print"));
        assert!(debug.contains("HOST_NAME"));
    }
    #[test]
    fn results_preserve_text_and_bound_binary_content() {
        let result = convert_result(
            json!({"content":[{"type":"text","text":"a"},{"type":"image","data":"YWJjZA=="},{"type":"text","text":"b"}],"structuredContent":{"ignored":true},"isError":true}),
        );
        assert_eq!(result.content, "a\n[MCP image: 4 bytes omitted]\nb");
        assert!(result.is_error);
        let result = convert_result(json!({"content":[],"structuredContent":{"x":2}}));
        assert_eq!(result.content, "{\"x\":2}");
        assert!(!result.is_error);
    }
    #[test]
    fn input_requests_and_unknown_result_types_are_not_completed_or_replayed() {
        for kind in ["input_required", "unrecognized"] {
            let result = convert_result(json!({"resultType":kind,"content":[],"inputRequests":[]}));
            assert!(result.is_error);
            assert!(result.content.contains("outcome unknown"));
            assert!(result.content.contains("Do not replay"));
        }
        let result = convert_result(
            json!({"resultType":"complete","content":[{"type":"text","text":"done"}]}),
        );
        assert_eq!(result.content, "done");
        assert!(!result.is_error);
    }
}
