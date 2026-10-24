//! Registry views: model and host restrictions, read-only subsets and the publish generation.
use crate::*;
impl ToolRegistry {
    /// Select the model entrypoints while preserving host capability policy.
    /// JavaScript mode exposes only `js`; Native never exposes that adapter.
    pub fn with_execution_mode(&self, mode: ExecutionMode) -> Self {
        let mut registry = self.clone();
        registry.promoted.clear();
        match mode {
            ExecutionMode::Js => registry.tools.retain(|name, _| name == "js"),
            ExecutionMode::Native => {
                registry.tools.remove("js");
                registry.host_tools.remove("js");
                crate::discovery::register_discovery_tools(&mut registry)
                    .expect("valid built-in discovery schemas");
                registry.promoted.extend(
                    registry
                        .host_tools
                        .keys()
                        .filter(|name| !registry.deferred.contains(*name))
                        .cloned(),
                );
            }
        }
        registry
    }

    /// Keep a capability callable and discoverable without advertising its full
    /// schema as a direct native tool. Names may be marked before registration.
    pub fn mark_deferred(&mut self, name: impl Into<String>) {
        let name = name.into();
        self.promoted.remove(&name);
        self.deferred.insert(name);
    }
    pub fn is_deferred(&self, name: &str) -> bool {
        self.deferred.contains(name)
    }
    pub fn set_deferred(&mut self, name: impl Into<String>, deferred: bool) {
        let name = name.into();
        if deferred {
            self.mark_deferred(name);
        } else {
            self.deferred.remove(&name);
        }
    }
    /// Original server identity, before any wire-name sanitization or truncation.
    pub fn set_tool_server(&mut self, name: impl Into<String>, server: impl Into<String>) {
        self.tool_servers.insert(name.into(), server.into());
    }
    pub fn tool_server(&self, name: &str) -> Option<&str> {
        self.tool_servers.get(name).map(String::as_str)
    }

    /// Select model entrypoints without changing the host capability policy.
    pub fn restricted(&self, names: &[String]) -> Self {
        Self {
            deferred: self.deferred.clone(),
            tool_servers: self.tool_servers.clone(),
            promoted: self
                .promoted
                .iter()
                .filter(|name| names.contains(name))
                .cloned()
                .collect(),
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| names.contains(name))
                .map(|(name, tool)| (name.clone(), tool.clone()))
                .collect(),
            host_tools: self.host_tools.clone(),
            validators: self.validators.clone(),
            shown_always: self.shown_always.clone(),
            unavailable: self.unavailable.clone(),
            result_limiter: self.result_limiter.clone(),
            generation: self.generation,
            model_allowlist: self.model_allowlist.clone(),
        }
    }
    /// Restrict host access without changing the model entrypoints.
    pub fn restricted_capabilities(&self, names: &[String]) -> Self {
        Self {
            deferred: self.deferred.clone(),
            tool_servers: self.tool_servers.clone(),
            promoted: self
                .promoted
                .iter()
                .filter(|name| names.contains(name))
                .cloned()
                .collect(),
            tools: self.tools.clone(),
            host_tools: self
                .host_tools
                .iter()
                .filter(|(name, _)| names.contains(name))
                .map(|(name, tool)| (name.clone(), tool.clone()))
                .collect(),
            validators: self.validators.clone(),
            shown_always: self
                .shown_always
                .iter()
                .filter(|name| names.contains(name))
                .cloned()
                .collect(),
            unavailable: self.unavailable.clone(),
            result_limiter: self.result_limiter.clone(),
            generation: self.generation,
            model_allowlist: self.model_allowlist.clone(),
        }
    }
    pub fn read_only(&self) -> Self {
        Self {
            deferred: self.deferred.clone(),
            tool_servers: self.tool_servers.clone(),
            promoted: self
                .promoted
                .iter()
                .filter(|name| {
                    self.host_tools
                        .get(*name)
                        .is_some_and(|tool| tool.definition().read_only)
                })
                .cloned()
                .collect(),
            tools: self
                .tools
                .iter()
                .filter(|(_, tool)| tool.definition().read_only)
                .map(|(name, tool)| (name.clone(), tool.clone()))
                .collect(),
            host_tools: self
                .host_tools
                .iter()
                .filter(|(_, tool)| tool.definition().read_only)
                .map(|(name, tool)| (name.clone(), tool.clone()))
                .collect(),
            validators: self.validators.clone(),
            shown_always: self.shown_always.clone(),
            unavailable: self.unavailable.clone(),
            result_limiter: self.result_limiter.clone(),
            generation: self.generation,
            model_allowlist: self.model_allowlist.clone(),
        }
    }
    /// Enforce a run's `tool_names` at call time. Declarations are not filtered by it.
    pub(crate) fn with_model_allowlist(mut self, names: Option<&[String]>) -> Self {
        self.model_allowlist = names.map(<[String]>::to_vec);
        self
    }
    /// Stamp the publish generation. A live republish bumps it so runs can see the change.
    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }
    /// The publish generation of this snapshot; 0 when never stamped.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// The single model-call gate, shared by model turns and `call_tool`. Entrypoints
    /// outside the run's allowlist are refused before any approval or effect.
    pub(crate) fn validate_model_call(&self, call: &ToolCall) -> anyhow::Result<()> {
        if self.get(&call.name).is_none() {
            return Err(self.unavailable_error(&call.name, "Unknown tool"));
        }
        if let Some(allowed) = &self.model_allowlist
            && !is_access_wrapper(&call.name)
            && !allowed.contains(&call.name)
        {
            anyhow::bail!(
                "Tool `{}` is not available in this run. Allowed tools: {}.",
                call.name,
                allowed_tools_phrase(allowed)
            );
        }
        self.validate(&call.name, &call.arguments)
    }
}
/// Access wrappers compose or discover targets; each target retains the run gate.
pub(crate) fn is_access_wrapper(name: &str) -> bool {
    matches!(name, "js" | "tool_search" | "call_tool")
}
/// Sorted, de-duplicated names for the run's allowed tools; `none` when empty.
pub(crate) fn allowed_tools_phrase(names: &[String]) -> String {
    let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}
