//! The registry of tools a run offers, with its validation and result limiter.
use crate::tool::{Tool, ToolContext};
use crate::tool_result::ToolResult;
use anyhow::Result;
use bubblepaw_ai::ToolDefinition;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Default)]
pub struct ToolRegistry {
    pub(crate) tools: BTreeMap<String, Arc<dyn Tool>>,
    pub(crate) host_tools: BTreeMap<String, Arc<dyn Tool>>,
    pub(crate) promoted: std::collections::BTreeSet<String>,
    pub(crate) deferred: std::collections::BTreeSet<String>,
    pub(crate) tool_servers: BTreeMap<String, String>,
    pub(crate) validators: BTreeMap<String, Arc<jsonschema::Validator>>,
    pub(crate) shown_always: std::collections::BTreeSet<String>,
    pub(crate) unavailable: BTreeMap<String, String>,
    pub(crate) result_limiter: Option<ToolResultLimiter>,
    pub(crate) generation: u64,
    /// Model entrypoints a run may call; declarations are not affected.
    pub(crate) model_allowlist: Option<Vec<String>>,
}
/// Optional host policy for preserving large results before the engine's hard cap.
pub type ToolResultLimiter =
    Arc<dyn Fn(&ToolContext, ToolResult, usize) -> Result<ToolResult> + Send + Sync>;
impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_result_limiter(&mut self, limiter: ToolResultLimiter) {
        self.result_limiter = Some(limiter);
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        self.register_tool(tool, false)
    }
    /// Register a host capability. It is callable through a model tool's dispatcher
    /// by default, and can also be exposed directly in native execution mode.
    pub fn register_host(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        self.register_tool(tool, true)
    }
    fn register_tool(&mut self, tool: Arc<dyn Tool>, host: bool) -> Result<()> {
        let definition = tool.definition();
        let name = definition.name;
        anyhow::ensure!(self.get(&name).is_none(), "Duplicate tool name: {name}");
        let validator = jsonschema::validator_for(&definition.input_schema)
            .map_err(|e| anyhow::anyhow!("Invalid tool schema for {name}: {e}"))?;
        self.unavailable.remove(&name);
        self.validators.insert(name.clone(), Arc::new(validator));
        if host {
            self.host_tools.insert(name, tool);
        } else {
            self.tools.insert(name, tool);
        }
        Ok(())
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.get_model(name).or_else(|| self.get_host(name))
    }
    /// Explain a removed capability without advertising it as available.
    /// The host builds a replacement snapshot before publishing it.
    pub fn remove(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        let name = name.into();
        self.tools.remove(&name);
        self.host_tools.remove(&name);
        self.promoted.remove(&name);
        self.deferred.remove(&name);
        self.tool_servers.remove(&name);
        self.validators.remove(&name);
        self.shown_always.remove(&name);
        self.unavailable.insert(name, reason.into());
    }
    pub(crate) fn unavailable_error(&self, name: &str, fallback: &str) -> anyhow::Error {
        match self.unavailable.get(name) {
            Some(reason) => anyhow::anyhow!("Tool {name} is unavailable: {reason}"),
            None => anyhow::anyhow!("{fallback}: {name}"),
        }
    }
    pub fn get_model(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned().or_else(|| {
            self.promoted
                .contains(name)
                .then(|| self.get_host(name))
                .flatten()
        })
    }
    pub fn get_host(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.host_tools.get(name).cloned()
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .chain(
                self.promoted
                    .iter()
                    .filter_map(|name| self.host_tools.get(name)),
            )
            .map(|tool| tool.definition_in(self))
            .collect()
    }
    /// Mark host capabilities whose full call signatures are always shown in
    /// a code tool's description. Others are listed by name and discovered on
    /// demand. Names need not be registered yet.
    pub fn show_always<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.shown_always.extend(names.into_iter().map(Into::into));
    }
    /// A registered host capability marked always-shown.
    pub fn is_shown_always(&self, name: &str) -> bool {
        self.shown_always.contains(name) && self.host_tools.contains_key(name)
    }
    /// Host capability definitions, including any also exposed in native mode.
    pub fn capabilities(&self) -> Vec<ToolDefinition> {
        self.host_tools
            .iter()
            .filter(|(name, _)| {
                self.model_allowlist.as_ref().is_none_or(|names| {
                    names.contains(name) || crate::tool_views::is_access_wrapper(name)
                })
            })
            .map(|(_, tool)| tool.definition())
            .collect()
    }
    pub(crate) fn validate(&self, name: &str, input: &Value) -> Result<()> {
        let tool = self
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {name}"))?;
        let validator = self
            .validators
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {name}"))?;
        validator.validate(input).map_err(|error| {
            // Prefer an actionable argument-name error even if a missing required
            // field or another invalid value was encountered first.
            let error = validator
                .iter_errors(input)
                .find(|error| {
                    matches!(
                        error.kind(),
                        jsonschema::error::ValidationErrorKind::AdditionalProperties { .. }
                    ) || (matches!(
                        error.kind(),
                        jsonschema::error::ValidationErrorKind::FalseSchema
                    ) && error
                        .schema_path()
                        .to_string()
                        .ends_with("/additionalProperties"))
                })
                .unwrap_or(error);
            let path = error.instance_path().to_string();
            let path = if path.is_empty() { "/" } else { &path };
            let schema = tool.definition().input_schema;
            let unexpected = match error.kind() {
                jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } => {
                    Some(unexpected.clone())
                }
                // A closed object without `properties` uses a false-schema
                // validator. Its displayed error would otherwise echo a value.
                jsonschema::error::ValidationErrorKind::FalseSchema
                    if error
                        .schema_path()
                        .to_string()
                        .ends_with("/additionalProperties") =>
                {
                    input
                        .pointer(&error.instance_path().to_string())
                        .and_then(Value::as_object)
                        .map(|object| object.keys().cloned().collect())
                }
                _ => None,
            };
            if let Some(unexpected) = unexpected {
                let schema_path = error.schema_path().to_string();
                let object = schema_path
                    .strip_suffix("/additionalProperties")
                    .and_then(|path| schema.pointer(path));
                let valid: Vec<_> = object
                    .and_then(|object| object["properties"].as_object())
                    .into_iter()
                    .flat_map(|properties| properties.keys().map(String::as_str))
                    .collect();
                return anyhow::anyhow!(
                    "Invalid arguments for {name} at {path}: unexpected arguments: {}. Valid arguments: {}.",
                    unexpected.join(", "),
                    if valid.is_empty() {
                        "(none)".to_owned()
                    } else {
                        valid.join(", ")
                    }
                );
            }
            let choices = schema
                .pointer(&error.schema_path().to_string())
                .and_then(Value::as_array);
            let types: Vec<_> = choices
                .into_iter()
                .flatten()
                .filter_map(|choice| choice["type"].as_str())
                .take(8)
                .collect();
            let hint = if types.is_empty() {
                String::new()
            } else {
                format!(" Expected one of: {}.", types.join(" or "))
            };
            let detail: String = error.to_string().chars().take(512).collect();
            anyhow::anyhow!("Invalid arguments for {name} at {path}: {detail}.{hint}")
        })
    }
    /// Explicitly release a session's foreground waits without cancelling work.
    /// Ordinary steering queues input and does not invoke this host control.
    pub fn interrupt_waits(&self, session: &str) -> usize {
        self.tools
            .values()
            .chain(self.host_tools.values())
            .map(|tool| tool.interrupt_wait(session))
            .sum()
    }
}
