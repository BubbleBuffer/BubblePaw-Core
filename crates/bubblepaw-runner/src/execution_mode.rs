//! Keep ordinary host instructions aligned with a chat's persisted execution mode.
use bubblepaw_core::{ExecutionMode, HarnessConfig, Store};
use std::sync::Arc;

/// Select default instructions by persisted mode before memory/skills are added.
/// Explicit run prompts pass through unchanged. Wrapping the existing resolver
/// also allows child profiles to replace their own base while retaining host context.
pub fn configure_prompts(config: &mut HarnessConfig, store: Store, native: String, js: String) {
    config.system_prompt = match config.execution_mode {
        ExecutionMode::Native => native.clone(),
        ExecutionMode::Js => js.clone(),
    };
    let configured_base = config.system_prompt.clone();
    let default_mode = config.execution_mode;
    let resolver = config.system_prompt_resolver.clone();
    config.system_prompt_resolver = Some(Arc::new(move |session, base| {
        let selected = if base == configured_base {
            // A read never binds: admission binds the mode with its accepted input.
            match store.candidate_execution_mode(session, default_mode)? {
                ExecutionMode::Native => native.as_str(),
                ExecutionMode::Js => js.as_str(),
            }
        } else {
            base
        };
        match &resolver {
            Some(resolve) => resolve(session, selected),
            None => Ok(selected.to_owned()),
        }
    }));
}

/// Fail startup if the selected adapter presents an inconsistent model surface.
pub fn validate_surface(
    tools: &bubblepaw_core::ToolRegistry,
    mode: ExecutionMode,
) -> anyhow::Result<()> {
    let definitions = tools.with_execution_mode(mode).definitions();
    match mode {
        ExecutionMode::Js => anyhow::ensure!(
            definitions.len() == 1 && definitions[0].name == "js",
            "JS execution mode must expose only js"
        ),
        ExecutionMode::Native => anyhow::ensure!(
            definitions.iter().all(|tool| tool.name != "js"),
            "Native execution mode must not expose js"
        ),
    }
    Ok(())
}
