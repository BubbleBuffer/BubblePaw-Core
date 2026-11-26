//! Registry views: model and host restrictions, read-only subsets and the publish generation.
use crate::execution_mode::ExecutionMode;
use crate::tool_registry::ToolRegistry;
use bubblepaw_ai::{ToolCall, ToolDefinition};
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
    /// Definitions of every registered tool, model entrypoints and host capabilities, by name.
    pub fn entries(&self) -> Vec<ToolDefinition> {
        let mut all = std::collections::BTreeMap::new();
        for tool in self.tools.values().chain(self.host_tools.values()) {
            let definition = tool.definition();
            all.entry(definition.name.clone()).or_insert(definition);
        }
        all.into_values().collect()
    }
    /// Whether a removal reason is recorded for the name. Built-in adapters do not
    /// register such a name again (the discovery tools in native mode).
    pub fn is_unavailable(&self, name: &str) -> bool {
        self.unavailable.contains_key(name)
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
    pub fn with_model_allowlist(mut self, names: Option<&[String]>) -> Self {
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
    /// outside the run's allowlist are refused before any permission check or effect.
    pub(crate) fn validate_model_call(&self, call: &ToolCall) -> anyhow::Result<()> {
        if self.get(&call.name).is_none() {
            return Err(self.unavailable_error(&call.name, "Unknown tool"));
        }
        self.check_model_allowlist(&call.name)?;
        self.validate(&call.name, &call.arguments)
    }
    fn check_model_allowlist(&self, name: &str) -> anyhow::Result<()> {
        if let Some(allowed) = &self.model_allowlist
            && !is_access_wrapper(name)
            && !allowed.iter().any(|allowed| allowed == name)
        {
            anyhow::bail!(
                "Tool `{name}` is not available in this run. Allowed tools: {}.",
                allowed_tools_phrase(allowed)
            );
        }
        Ok(())
    }
    pub(crate) fn validate_direct_model_call(
        &self,
        call: &ToolCall,
        declarations: &[String],
    ) -> Result<(), (anyhow::Error, bool)> {
        let Some(tool) = self.get(&call.name) else {
            let error = self.unavailable_error(&call.name, "Unknown tool");
            return Err((
                self.with_tool_suggestions(error, &call.name, declarations),
                true,
            ));
        };
        self.check_model_allowlist(&call.name)
            .map_err(|error| (error, false))?;
        if call.name != crate::submit_result::NAME && !declarations.contains(&call.name) {
            let error = anyhow::anyhow!("Tool `{}` was not declared for this request.", call.name);
            return Err((
                self.with_tool_suggestions(error, &call.name, declarations),
                true,
            ));
        }
        let result = if call.arguments.is_object() {
            self.validate(&call.name, &call.arguments)
        } else if let Some(raw) = call.arguments.as_str()
            && let Err(error) = serde_json::from_str::<serde_json::Value>(raw)
        {
            Err(anyhow::anyhow!(
                "Invalid arguments for {}: arguments were not valid JSON ({error}).",
                call.name
            ))
        } else {
            Err(anyhow::anyhow!(
                "Invalid arguments for {}: expected a JSON object.",
                call.name
            ))
        };
        result.map_err(|error| {
            let schema = tool.definition().input_schema;
            let parameters = schema["properties"]
                .as_object()
                .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let required = schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>();
            let error = if parameters.is_empty() {
                error
            } else {
                anyhow::anyhow!(
                    "{error} Expected parameters: {}. Required: {}.",
                    parameters.join(", "),
                    if required.is_empty() {
                        "none".into()
                    } else {
                        required.join(", ")
                    }
                )
            };
            (error, true)
        })
    }
    fn with_tool_suggestions(
        &self,
        error: anyhow::Error,
        name: &str,
        declarations: &[String],
    ) -> anyhow::Error {
        let name: Vec<_> = name.chars().take(128).collect();
        let mut candidates: Vec<_> = declarations
            .iter()
            .filter(|name| self.get(name).is_some() && self.check_model_allowlist(name).is_ok())
            .map(|candidate| (tool_name_distance(&name, candidate), candidate.as_str()))
            .collect();
        candidates.sort_unstable();
        let closest: Vec<_> = candidates
            .into_iter()
            .take(3)
            .map(|(_, name)| name)
            .collect();
        if closest.is_empty() {
            error
        } else {
            anyhow::anyhow!("{error} Closest available tools: {}.", closest.join(", "))
        }
    }
}

fn tool_name_distance(name: &[char], candidate: &str) -> usize {
    let mut costs: Vec<_> = (0..=name.len()).collect();
    for (row, candidate_char) in candidate.chars().take(128).enumerate() {
        let mut diagonal = costs[0];
        costs[0] = row + 1;
        for (column, &name_char) in name.iter().enumerate() {
            let above = costs[column + 1];
            costs[column + 1] = (diagonal + usize::from(name_char != candidate_char))
                .min(1 + above.min(costs[column]));
            diagonal = above;
        }
    }
    costs[name.len()]
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
