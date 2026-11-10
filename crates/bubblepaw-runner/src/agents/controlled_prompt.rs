//! The turn prompt with its tool-call budget. Only a Js session has cells to mention.
use super::Controls;
use anyhow::Result;
use bubblepaw_core::{ExecutionMode, Harness};

pub(super) fn controlled_prompt(
    text: &str,
    controls: &Controls,
    harness: &Harness,
    session: &str,
) -> Result<String> {
    let Some(max) = controls.max_tool_calls else {
        return Ok(text.to_owned());
    };
    let nested = match harness.execution_mode(session)? {
        ExecutionMode::Js => ", including each call made inside a js cell",
        ExecutionMode::Native => "",
    };
    Ok(format!(
        "{text}\n\nYou have {max} tool calls for this turn. Every tool call counts{nested}. Plan within the budget and return your final answer when it is exhausted."
    ))
}
