//! The per-run options the loop reads. The shell builds this once per run from
//! its own options; the loop knows no other option.
use bubblepaw_ai::JsonSchema;

#[derive(Debug, Clone, Default)]
pub struct LoopOptions {
    /// The model for this run's requests; None uses the provider resolver's default.
    pub model: Option<String>,
    /// Overrides the default round limit for this run.
    pub max_rounds: Option<usize>,
    pub max_output_tokens: Option<u32>,
    /// Shared budget for model tool calls and nested host invocations. None is unlimited.
    pub max_tool_calls: Option<usize>,
    /// Required structured result, submitted through the run's `submit_result` tool.
    pub output_schema: Option<JsonSchema>,
    /// Run this `js` code as the run's single tool round instead of calling a model.
    pub script: Option<String>,
}
