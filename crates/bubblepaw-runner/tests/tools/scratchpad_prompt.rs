use anyhow::Result;
use bubblepaw_core::{ContextStrategy, ExecutionMode, HarnessConfig, Store};
use bubblepaw_runner::{
    execution_mode::configure_prompts,
    root_prompt::{configure_scratchpads, root_system_prompt},
    tools::WorkspaceTools,
};

#[test]
fn prompts_show_granted_absolute_paths_and_child_subfolder_in_both_modes() -> Result<()> {
    let data = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?;
    let child =
        store.create_session_with_parent("child", ContextStrategy::default(), Some(&parent.id))?;
    let workspace = WorkspaceTools::new(root.path())?;
    workspace.prepare_scratchpads(data.path())?;
    let parent_paths = workspace.scratchpads(&store, &parent.id)?;
    for mode in [ExecutionMode::Native, ExecutionMode::Js] {
        let mut config = HarnessConfig {
            execution_mode: mode,
            ..Default::default()
        };
        configure_prompts(
            &mut config,
            store.clone(),
            root_system_prompt(root.path(), None, ExecutionMode::Native),
            root_system_prompt(root.path(), None, ExecutionMode::Js),
        );
        configure_scratchpads(&mut config, store.clone(), workspace.clone());
        let resolve = config.system_prompt_resolver.as_ref().unwrap();
        let prompt = resolve(&parent.id, &config.system_prompt)?;
        assert!(prompt.contains(&parent_paths.chat.display().to_string()));
        assert!(prompt.contains(&parent_paths.workspace.display().to_string()));
        assert!(prompt.contains("parts:[\"python \",\"\"]"));
        assert!(prompt.contains("no python -c, node -e or inline PowerShell"));
        assert!(prompt.contains("-NoProfile -File"));
        assert!(prompt.contains(bubblepaw_runner::command_template::shell_name()));
        let prompt = resolve(&child.id, "Custom child instructions.")?;
        assert!(prompt.contains("Custom child instructions."));
        assert!(prompt.contains(&parent_paths.chat.join(&child.id).display().to_string()));
        let child_workspace = WorkspaceTools::new(other.path())?;
        child_workspace.prepare_scratchpads(data.path())?;
        let child_paths = child_workspace.scratchpads(&store, &child.id)?;
        configure_scratchpads(&mut config, store.clone(), child_workspace);
        let prompt = config.system_prompt_resolver.as_ref().unwrap()(&child.id, "Child worktree.")?;
        assert_eq!(prompt.matches("<scratchpads>").count(), 1);
        assert!(prompt.contains(&parent_paths.chat.display().to_string()));
        assert!(prompt.contains(&child_paths.workspace.display().to_string()));
        assert!(!prompt.contains(&parent_paths.workspace.display().to_string()));
    }
    Ok(())
}
