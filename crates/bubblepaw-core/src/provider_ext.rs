//! Tool-surface helpers over the provider extension types (docs/providers.md).
use crate::{ModelRequest, ToolRegistry, ToolRegistryResolver, ToolSurface};
use std::sync::Arc;

/// The code-mode entry point, the only tool a `Code` surface offers.
pub const CODE_TOOL: &str = "js";

/// Apply a provider's tool surface to one prepared request. Registration and
/// direct-call validation are unaffected; only the advertised list changes.
pub fn apply_tool_surface(request: &mut ModelRequest, surface: &ToolSurface) {
    // Without `js` among the request's tools, Code falls back to Direct.
    if *surface == ToolSurface::Code && request.tools.iter().any(|tool| tool.name == CODE_TOOL) {
        request.tools.retain(|tool| tool.name == CODE_TOOL);
    }
}

/// Wrap a source resolver so each snapshot passes through `derive`. The
/// closure runs after `source()`, once per resolution.
pub fn derived_registry_resolver(
    source: ToolRegistryResolver,
    derive: impl Fn(ToolRegistry) -> ToolRegistry + Send + Sync + 'static,
) -> ToolRegistryResolver {
    Arc::new(move || derive(source()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolDefinition;

    #[test]
    fn code_surface_keeps_only_js_and_falls_back_without_it() {
        let tool = |name: &str| ToolDefinition {
            approval: Default::default(),
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: true,
        };
        let mut request = ModelRequest {
            tools: vec![tool("js"), tool("history_search"), tool("other")],
            ..Default::default()
        };
        let surface = ToolSurface::Code;
        apply_tool_surface(&mut request, &ToolSurface::Direct);
        assert_eq!(request.tools.len(), 3);
        apply_tool_surface(&mut request, &surface);
        let names: Vec<_> = request.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["js"]);
        let mut without_js = ModelRequest {
            tools: vec![tool("history_search"), tool("other")],
            ..Default::default()
        };
        apply_tool_surface(&mut without_js, &surface);
        assert_eq!(without_js.tools.len(), 2);
    }
}
