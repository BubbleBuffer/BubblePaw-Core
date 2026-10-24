//! Live inventory notices and the catalog served only after a context apply.
use super::*;
use crate::store::tool_surface::{CatalogTool, ChatToolSurface};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn catalog(tools: &ToolRegistry) -> Vec<CatalogTool> {
    let mut definitions: BTreeMap<_, _> = tools
        .capabilities()
        .into_iter()
        .chain(tools.definitions())
        .filter(|tool| tool.name != "js" && tool.name != crate::submit_result::NAME)
        .map(|tool| (tool.name.clone(), tool))
        .collect();
    definitions
        .iter_mut()
        .map(|(name, definition)| CatalogTool {
            definition: definition.clone(),
            deferred: tools.is_deferred(name),
            server: tools.tool_server(name).map(str::to_owned),
        })
        .collect()
}

pub(super) fn same_catalog(left: &[CatalogTool], right: &[CatalogTool]) -> bool {
    serde_json::to_value(left).ok() == serde_json::to_value(right).ok()
}

fn display(tool: &CatalogTool) -> Value {
    if tool.deferred {
        json!({"name":tool.definition.name,"server":tool.server,"deferred":true})
    } else {
        serde_json::to_value(&tool.definition).expect("tool definitions serialize")
    }
}

fn access_instructions(tools: &ToolRegistry) -> &'static str {
    if tools.get("js").is_some() {
        "Discover deferred schemas with tools.$search(query), then invoke tools.<name>(arguments). Use tools.<name>(arguments) for newly available or updated core tools."
    } else {
        "Discover deferred schemas with tool_search, then invoke with call_tool. Use call_tool for newly available or updated core tools."
    }
}

pub(super) fn notice(
    before: &[CatalogTool],
    after: &[CatalogTool],
    tools: &ToolRegistry,
) -> Result<String> {
    let before: BTreeMap<_, _> = before
        .iter()
        .map(|tool| (&tool.definition.name, tool))
        .collect();
    let after: BTreeMap<_, _> = after
        .iter()
        .map(|tool| (&tool.definition.name, tool))
        .collect();
    let added: Vec<_> = after
        .iter()
        .filter(|(name, _)| !before.contains_key(*name))
        .map(|(_, tool)| display(tool))
        .collect();
    let removed: Vec<_> = before
        .keys()
        .filter(|name| !after.contains_key(*name))
        .collect();
    let updated: Vec<_> = after
        .iter()
        .filter(|(name, tool)| {
            before.get(*name).is_some_and(|old| {
                serde_json::to_value(old).ok() != serde_json::to_value(tool).ok()
            })
        })
        .map(|(_, tool)| display(tool))
        .collect();
    Ok(format!(
        "Tool inventory changed.\n{}\n{} The chat's declared tools and system prompt stay fixed.",
        serde_json::to_string(&json!({"added":added,"removed":removed,"updated":updated}))?,
        access_instructions(tools),
    ))
}

pub(super) fn cold_catalog(surface: &ChatToolSurface, tools: &ToolRegistry) -> Result<String> {
    let current = catalog(tools);
    let initial: BTreeMap<_, _> = surface
        .initial_catalog
        .iter()
        .map(|tool| (&tool.definition.name, tool))
        .collect();
    let added: Vec<_> = current
        .iter()
        .filter(|tool| !initial.contains_key(&tool.definition.name))
        .map(display)
        .collect();
    let removed: Vec<_> = initial
        .keys()
        .filter(|name| !current.iter().any(|tool| &tool.definition.name == **name))
        .collect();
    let updated: Vec<_> = current
        .iter()
        .filter(|tool| {
            initial.get(&tool.definition.name).is_some_and(|old| {
                serde_json::to_value(old).ok() != serde_json::to_value(tool).ok()
            })
        })
        .map(display)
        .collect();
    serde_json::to_string(&json!({
        "anchors":super::runtime_tools::deferred_anchors(tools), "added":added, "removed":removed, "updated":updated,
        "instructions":access_instructions(tools)
    }))
    .map_err(Into::into)
}

impl Harness {
    /// The catalog is a cold note like any other: it is staged through the
    /// session's ContextWriter, after the host hook's own notes.
    pub(super) fn stage_chat_catalog(
        &self,
        session: &str,
        writer: &crate::ContextWriter<'_>,
    ) -> Result<()> {
        if let Some(text) = self
            .store
            .chat_tool_surface(session)?
            .and_then(|surface| surface.cold_catalog)
        {
            writer.cold_note("tool_catalog", &text);
        }
        Ok(())
    }

    pub(super) fn update_applied_catalog(&self, session: &str, tools: &ToolRegistry) -> Result<()> {
        let Some(surface) = self.store.chat_tool_surface(session)? else {
            return Ok(());
        };
        let text = cold_catalog(&surface, tools)?;
        if surface.cold_catalog.as_ref() != Some(&text) {
            self.store.update_chat_tool_surface(session, |surface| {
                surface.cold_catalog = Some(text);
                Ok(())
            })?;
        }
        let writer = crate::ContextWriter::new(self.context_state(), session);
        self.stage_chat_catalog(session, &writer)
    }
}

#[cfg(test)]
#[path = "runtime_catalog_tests.rs"]
mod tests;
