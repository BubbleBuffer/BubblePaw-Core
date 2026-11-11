//! The agent tool surface and its manager.
use super::*;

#[derive(Clone)]
pub(crate) enum Manager {
    Strong(Arc<AgentTools>),
    Weak(Weak<AgentTools>),
}
impl Manager {
    pub(crate) fn get(&self) -> Result<Arc<AgentTools>> {
        match self {
            Self::Strong(m) => Ok(m.clone()),
            Self::Weak(m) => m.upgrade().ok_or_else(|| anyhow!("agent host unavailable")),
        }
    }
}
pub(super) struct AgentTool {
    pub(super) manager: Manager,
    pub(super) name: &'static str,
    pub(super) definition: OnceLock<ToolDefinition>,
}
#[async_trait]
impl Tool for AgentTool {
    fn definition(&self) -> ToolDefinition {
        // Registration pins the definition to this registry snapshot. Live
        // reloads re-register controls; other turns keep their original schema.
        self.definition.get_or_init(|| {
            // A child's weak manager can be gone only during host teardown.
            let (limits, profiles) = match self.manager.get() {
                Ok(manager) => (manager.limits(), manager.profile_summaries()),
                Err(_) => (Agents::default(), Vec::new()),
            };
            // Registration compiles each input validator from this schema once. A
            // child registered while profiles are still being built therefore has
            // no profile enum; start still rejects unknown names.
            let mut profile_schema = json!({"type":"string","default":"read-only"});
            if !profiles.is_empty() {
                profile_schema["enum"] = json!(profiles.iter().map(|(n, _)| n).collect::<Vec<_>>());
            }
            let (mut description,mut schema)=match self.name {
                "agent"=>(format!("Start a reusable child session with explicit task/context and a registered profile. Returns handle and sessionId; wait or agent_status reads its answer. Default profile read-only. Depth limit {}; active limits {}/parent, {}/host. Profiles: {}",cap(limits.max_depth),cap(limits.max_active_per_parent),cap(limits.max_active_total),profiles.iter().map(|(n,d)| format!("{n}: {d}")).collect::<Vec<_>>().join("; ")),json!({"type":"object","properties":{"task":{"type":"string","minLength":1},"context":{},"profile":profile_schema},"required":["task"],"additionalProperties":false})),
                "agent_send"=>("Send text to an owned child by handle or sessionId. Steers running children immediately; starts a new turn in idle children.".into(),json!({"type":"object","properties":{"handle":{"type":"string"},"sessionId":{"type":"string"},"text":{"type":"string","minLength":1}},"required":["text"],"additionalProperties":false})),
                _=>("Report an owned child and consume its unread result, or list the descendant tree with parentSessionId and unread flags when no handle/sessionId is supplied. Listing does not consume results.".into(),json!({"type":"object","properties":{"handle":{"type":"string"},"sessionId":{"type":"string"}},"additionalProperties":false})),
            };
            if self.name == "agent" {
                schema["properties"]["worktree"] = json!({"type":"boolean","default":false});
                description.push_str(" worktree:true opts into a git worktree on bp/<sessionId> from the parent's current HEAD. Returns workspace, branch and worktree.path. Parent uncommitted changes are not copied. Default false.");
            }
            if self.name == "agent_send" {
                schema["properties"]["question_id"] = json!({"type":"string","minLength":1});
                description.push_str(" When answering a pending question, include question_id to correlate the answer. A question already answered by the user rejects a late answer with that id.");
            }
            if matches!(self.name, "agent" | "agent_send") {
                description.push_str(" Optional output_schema {name,schema} declares submit_result with that input schema; finish by calling it. Invalid arguments return a tool error; a missing submission gets one text-only reminder before failure. Completed results include json and text. max_tool_calls bounds this run, counting model calls and each nested tool call; child limits cannot exceed the calling turn's remaining budget or timeout. Omission inherits those bounds, otherwise unlimited. timeout is a nonnegative integer wall-clock limit in milliseconds, cancelling and cleaning up only this turn. Every finished child can receive a new turn with its full history. Follow-up budgets and timeouts are fresh; output_schema is retained unless replaced or cleared with null. Controls cannot be changed while steering an active turn.");
                schema["properties"]["output_schema"] = json!({"anyOf":[{"type":"object","properties":{"name":{"type":"string"},"schema":{"type":"object"}},"required":["name","schema"],"additionalProperties":false},{"type":"null"}]});
                schema["properties"]["max_tool_calls"] = json!({"type":"integer","minimum":0});
                schema["properties"]["timeout"] = json!({"type":"integer","minimum":0});
            }
            ToolDefinition {
                approval: Default::default(),
                name: self.name.into(),
                description,
                input_schema: schema,
                read_only: self.name == "agent_status",
            }
        }).clone()
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        self.call(context, input, None)
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        self.call(context, input, Some(dispatcher))
    }
    fn cancel_session(&self, session: &str) -> usize {
        if self.name == "agent" {
            self.manager
                .get()
                .map(|m| m.cancel_session(session))
                .unwrap_or(0)
        } else {
            0
        }
    }
    fn take_reminders(&self, session: &str) -> Vec<bubblepaw_core::Reminder> {
        if self.name == "agent" {
            self.manager
                .get()
                .map(|manager| manager.take_reminders(session))
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}
impl AgentTool {
    fn call(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: Option<ToolDispatcher>,
    ) -> Result<ToolResult> {
        let result = (|| {
            let manager = self.manager.get()?;
            match self.name {
                "agent" => manager
                    .start_owned(&context, input, dispatcher)
                    .map(|v| (v, true)),
                "agent_send" => manager.send_owned(&context, input, dispatcher),
                _ => manager
                    .status(&context.session_id, &input)
                    .map(|v| (v, false)),
            }
        })();
        // Admission rejection is a known no-effect outcome, so the dispatcher's
        // ordinary completion path must settle it, rather than mark it unknown.
        Ok(match result {
            Ok((v, true)) => ToolResult::pending(v.to_string()),
            Ok((v, false)) => ToolResult::text(v.to_string()),
            Err(e) => ToolResult::error(e.to_string()),
        })
    }
}
