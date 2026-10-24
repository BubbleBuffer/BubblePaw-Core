//! Shared wait/cancel entry points for process and agent handles.
use crate::agents::{AgentTools, Manager};
use anyhow::{Result, bail, ensure};
use async_trait::async_trait;
use bubblepaw_core::{Tool, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn register_agent_handles(registry: &mut ToolRegistry, agents: Arc<AgentTools>) -> Result<()> {
    register_handles(registry, Manager::Strong(agents))
}

pub fn register_agent_handles_weak(
    registry: &mut ToolRegistry,
    agents: &Arc<AgentTools>,
) -> Result<()> {
    register_handles(registry, Manager::Weak(Arc::downgrade(agents)))
}

fn register_handles(registry: &mut ToolRegistry, agents: Manager) -> Result<()> {
    let wait = registry.get_host("wait");
    let cancel = registry.get_host("cancel");
    let retained = registry
        .capabilities()
        .into_iter()
        .filter(|tool| tool.name != "wait" && tool.name != "cancel")
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    *registry = registry.restricted_capabilities(&retained);
    for (name, fallback) in [("wait", wait), ("cancel", cancel)] {
        registry.register_host(Arc::new(HandleTool {
            name,
            agents: agents.clone(),
            fallback,
        }))?;
    }
    Ok(())
}

struct HandleTool {
    name: &'static str,
    agents: Manager,
    fallback: Option<Arc<dyn Tool>>,
}

#[async_trait]
impl Tool for HandleTool {
    fn definition(&self) -> ToolDefinition {
        let properties = if self.name == "wait" {
            json!({"ms":{"type":"integer","minimum":0},"handle":{"type":"string"},"sessionId":{"type":"string"},"timeoutMs":{"type":"integer","minimum":0}})
        } else {
            json!({"handle":{"type":"string"},"sessionId":{"type":"string"}})
        };
        ToolDefinition {
            approval: Default::default(),
            name: self.name.into(),
            description: if self.name == "wait" {
                "Wait for ms, an owned process/agent handle, or a child sessionId. timeoutMs returns timeout without cancelling work."
            } else { "Cancel an owned process/agent handle or child sessionId and await subtree cleanup." }.into(),
            input_schema: json!({"type":"object","properties":properties,"required":Vec::<String>::new(),"additionalProperties":false}),
            read_only: self.name == "wait",
        }
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        self.route(context, input, None).await
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        self.route(context, input, Some(dispatcher)).await
    }
}

impl HandleTool {
    async fn route(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: Option<ToolDispatcher>,
    ) -> Result<ToolResult> {
        if input.get("sessionId").is_some()
            || input["handle"]
                .as_str()
                .is_some_and(|h| h.starts_with("agent:"))
        {
            ensure!(
                input.get("sessionId").is_none() || input.get("handle").is_none(),
                "provide either handle or sessionId"
            );
            let handle = input
                .get("sessionId")
                .or_else(|| input.get("handle"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("handle or sessionId must be a string"))?;
            ensure!(
                input.as_object().is_some_and(|o| o
                    .keys()
                    .all(|k| matches!(k.as_str(), "handle" | "sessionId" | "timeoutMs"))),
                "unexpected agent handle argument"
            );
            if let Some(timeout) = input.get("timeoutMs") {
                ensure!(
                    self.name == "wait" && timeout.as_u64().is_some(),
                    "timeoutMs must be a nonnegative integer for wait"
                );
            }
            let agents = self.agents.get()?;
            ensure!(
                input.get("ms").is_none(),
                "wait accepts either ms or handle with optional timeoutMs"
            );
            let operation = async {
                if self.name == "wait" {
                    agents
                        .wait(&context.session_id, handle, input["timeoutMs"].as_u64())
                        .await
                } else {
                    agents.cancel(&context.session_id, handle).await
                }
            };
            let value = tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => bail!("Agent handle operation cancelled"),
                value = operation => value?,
            };
            return Ok(ToolResult::text(value.to_string()));
        }
        let fallback = self.fallback.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Process {} is unavailable in this workspace policy",
                self.name
            )
        })?;
        match dispatcher {
            Some(dispatcher) => {
                fallback
                    .invoke_with_dispatcher(context, input, dispatcher)
                    .await
            }
            None => fallback.invoke(context, input).await,
        }
    }
}
