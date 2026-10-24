//! The chat's immutable provider prefix and its independently refreshed inventory.
use super::*;
use crate::ToolDefinition;
use crate::store::tool_surface::ChatToolSurface;
use std::collections::BTreeMap;

pub(super) struct RunToolSurface {
    saved: ChatToolSurface,
}

impl RunToolSurface {
    pub(super) fn load(harness: &Harness, session: &str, options: &RunOptions) -> Result<Self> {
        Ok(Self {
            saved: harness.chat_surface(session, options, true)?,
        })
    }
    pub(super) fn declarations(&self) -> Vec<ToolDefinition> {
        self.saved.declarations.clone()
    }
    pub(super) fn prompt(&self) -> String {
        self.saved.system_prompt.clone()
    }
    pub(super) fn restore(&self, request: &mut ModelRequest) {
        request.system_prompt = self.prompt();
        request.tools = self.declarations();
        request.disabled_tools.clear();
        request.output_schema = None;
    }
}

fn with_anchors(mut prompt: String, anchors: &str) -> String {
    if !anchors.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(anchors);
    }
    prompt
}

pub(super) fn deferred_anchors(tools: &ToolRegistry) -> String {
    let mut servers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for tool in tools
        .capabilities()
        .into_iter()
        .filter(|tool| tools.is_deferred(&tool.name))
    {
        if let Some(server) = tools.tool_server(&tool.name) {
            servers
                .entry(server.to_owned())
                .or_default()
                .push(tool.name);
        }
    }
    if servers.is_empty() {
        return String::new();
    }
    let js = tools.get("js").is_some();
    let mut lines = vec![if js {
        "Deferred MCP tools (discover schemas with tools.$search, then invoke tools.<name>):".into()
    } else {
        "Deferred MCP tools (discover schemas with tool_search, then invoke with call_tool):".into()
    }];
    for (server, mut names) in servers {
        names.sort();
        let query = serde_json::to_string(&server).expect("server names serialize");
        let access = if js {
            format!("Use tools.$search({query}), then tools.<name>(arguments).")
        } else {
            format!("Use tool_search({query}), then call_tool(name, arguments).")
        };
        lines.push(format!(
            "- {server}: {} tools ({}{}). {access}",
            names.len(),
            names.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
            if names.len() > 3 { ", ..." } else { "" }
        ));
    }
    lines.join("\n")
}

impl Harness {
    /// Missing snapshots on legacy chats bind from their first use; an existing
    /// snapshot is never replaced by a config reload, model switch or context apply.
    pub(super) fn initial_chat_surface(
        &self,
        session: &str,
        options: &RunOptions,
    ) -> Result<Option<ChatToolSurface>> {
        if self.store.chat_tool_surface(session)?.is_some() {
            return Ok(None);
        }
        let mode = self
            .store
            .candidate_execution_mode(session, self.config.execution_mode)?;
        let tools = self.selected_tools_in_mode(session, options, mode)?;
        let base = options
            .system_prompt
            .as_deref()
            .unwrap_or(&self.config.system_prompt);
        let resolved = match &self.config.system_prompt_resolver {
            Some(resolve) => resolve(session, base)?,
            None => base.to_owned(),
        };
        let system_prompt = if mode == crate::ExecutionMode::Native {
            with_anchors(resolved, &deferred_anchors(&tools))
        } else {
            resolved
        };
        let (model, provider) = self.provider_binding();
        let mut request = ModelRequest {
            model: options.model.clone().unwrap_or(model),
            system_prompt,
            tools: tools.definitions(),
            ..Default::default()
        };
        if let Some(schema) = &options.output_schema {
            crate::submit_result::restore_declaration(&mut request, &schema.schema, mode);
        }
        if mode == crate::ExecutionMode::Js {
            let surface = provider.capabilities(&request.model).tool_surface;
            crate::apply_tool_surface(&mut request, &surface);
        }
        let catalog = super::runtime_catalog::catalog(&tools);
        Ok(Some(ChatToolSurface {
            system_prompt: request.system_prompt,
            declarations: request.tools,
            initial_result: tools
                .get(crate::submit_result::NAME)
                .map(|tool| tool.definition()),
            result_contract_changed: false,
            initial_catalog: catalog.clone(),
            observed_catalog: catalog,
            inventory_notice: None,
            inventory_notice_base: None,
            cold_catalog: None,
        }))
    }

    pub(super) fn chat_surface(
        &self,
        session: &str,
        options: &RunOptions,
        bind: bool,
    ) -> Result<ChatToolSurface> {
        if let Some(surface) = self.store.chat_tool_surface(session)? {
            return Ok(surface);
        }
        let candidate = self
            .initial_chat_surface(session, options)?
            .context("Chat tool surface disappeared")?;
        if bind {
            self.store.bind_chat_tool_surface(session, &candidate)
        } else {
            Ok(candidate)
        }
    }

    pub(super) fn refresh_run_tools(
        &self,
        session: &str,
        options: &RunOptions,
        _previous: &ToolRegistry,
    ) -> Result<ToolRegistry> {
        let tools = self.run_tool_snapshot(session, options)?;
        if let Some(mut surface) = self.store.chat_tool_surface(session)? {
            let current = super::runtime_catalog::catalog(&tools);
            if !super::runtime_catalog::same_catalog(&surface.observed_catalog, &current) {
                let last_notice = self
                    .store
                    .last_reminders(session, "root")?
                    .into_iter()
                    .find(|record| record.kind == "tools_reloaded");
                let delivered = surface.inventory_notice.as_ref().is_some_and(|notice| {
                    last_notice.as_ref().is_some_and(|record| {
                        record.text_sha256 == crate::fingerprint(notice.as_bytes())
                    })
                });
                surface = self.store.update_chat_tool_surface(session, |surface| {
                    // Only a canonical reminder commit acknowledges the target.
                    // Otherwise coalesce further changes from the retained base,
                    // so a restart cannot discard an undelivered earlier delta.
                    let base = if delivered || surface.inventory_notice.is_none() {
                        surface.observed_catalog.clone()
                    } else {
                        surface
                            .inventory_notice_base
                            .clone()
                            .unwrap_or_else(|| surface.initial_catalog.clone())
                    };
                    surface.inventory_notice =
                        Some(super::runtime_catalog::notice(&base, &current, &tools)?);
                    surface.inventory_notice_base = Some(base);
                    surface.observed_catalog = current;
                    Ok(())
                })?;
            }
            // Persisting the pending notice with the observed inventory makes a
            // restart before its ordinary reminder commit recoverable. The
            // authoritative reminder metadata suppresses redelivery after apply.
            if let Some(notice) = &surface.inventory_notice {
                self.context_state()
                    .remind(session, "root", "tools_reloaded", notice);
            }
        }
        Ok(tools)
    }

    pub(super) fn apply_tool_surface_change(
        &self,
        session: &str,
        options: &RunOptions,
        tools: &mut ToolRegistry,
        surface: &mut RunToolSurface,
        request: &mut ModelRequest,
    ) -> Result<bool> {
        let Some(reason) = self.config.context.take_prefix_change(session) else {
            return Ok(false);
        };
        self.declare_context_change(session, &reason);
        *tools = self.refresh_run_tools(session, options, tools)?;
        self.update_applied_catalog(session, tools)?;
        self.refresh_result_schema_notice(session, options, tools)?;
        surface.restore(request);
        Ok(true)
    }
}
