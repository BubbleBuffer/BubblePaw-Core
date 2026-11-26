//! The `initialize` result of an extension: its models and the protocol options
//! it supports.
use crate::Capabilities;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(super) struct InitializedModel {
    pub(super) id: String,
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) capabilities: Capabilities,
}
#[derive(Debug, Clone, Deserialize)]
pub(super) struct Initialized {
    #[serde(default)]
    pub(super) models: Vec<InitializedModel>,
    #[serde(default)]
    pub(super) capabilities: ExtensionCapabilities,
}
/// Request options supported by the extension protocol implementation, separate
/// from each model's core capabilities.
#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct ExtensionCapabilities {
    #[serde(default)]
    pub(super) reasoning_effort: bool,
    /// The extension answers `summary_fork` (see `fork.rs`).
    #[serde(default)]
    pub(super) summary_fork: bool,
    /// Tokens the extension adds in front of every conversation (for example
    /// Claude Code's own system prompt), counted into a fork's size estimate.
    #[serde(default)]
    pub(super) input_overhead_tokens: usize,
}
