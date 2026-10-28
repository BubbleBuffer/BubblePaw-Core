//! Bind root default instructions to the mode persisted for each session.
use super::*;

pub(super) fn configure(
    config: &mut HarnessConfig,
    store: &Store,
    workspace: &std::path::Path,
    data_dir: &std::path::Path,
    settings: &bubblepaw_runner::config::RootAgentSettings,
) -> Result<()> {
    bubblepaw_runner::execution_mode::configure_prompts(
        config,
        store.clone(),
        root_system_prompt(
            workspace,
            settings.system_prompt.as_deref(),
            ExecutionMode::Native,
        ),
        root_system_prompt(
            workspace,
            settings.system_prompt.as_deref(),
            ExecutionMode::Js,
        ),
    );
    let workspace = bubblepaw_runner::tools::WorkspaceTools::new(workspace)?;
    workspace.prepare_scratchpads(data_dir)?;
    bubblepaw_runner::root_prompt::configure_scratchpads(config, store.clone(), workspace);
    Ok(())
}
