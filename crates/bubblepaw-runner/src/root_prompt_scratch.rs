use crate::tools::WorkspaceTools;
use bubblepaw_core::{HarnessConfig, Store};
use std::sync::Arc;

const OPEN: &str = "\n<scratchpads>\n";
const CLOSE: &str = "\n</scratchpads>";

/// Bind the prompt's scratch paths to the same workspace/data directory as the
/// file tools. Children retain their root chat directory and replace only the
/// workspace grant when their host binds a separate worktree.
pub fn configure_scratchpads(config: &mut HarnessConfig, store: Store, workspace: WorkspaceTools) {
    let previous = config.system_prompt_resolver.clone();
    config.system_prompt_resolver = Some(Arc::new(move |session, base| {
        let mut prompt = match &previous {
            Some(resolve) => resolve(session, base)?,
            None => base.to_owned(),
        };
        // A child profile inherits the parent's resolver, then binds its own
        // effective workspace. Keep one host-owned paths block in that case.
        if let Some(start) = prompt.find(OPEN)
            && let Some(end) = prompt[start..].find(CLOSE)
        {
            prompt.replace_range(start..start + end + CLOSE.len(), "");
        }
        let paths = workspace.scratchpads(&store, session)?;
        prompt.push_str(&format!(
            "{OPEN}Chat scratchpad: {}\nProject scratchpad: {}\nUse these absolute paths with read, write, edit and exec workdir.",
            paths.chat.display(), paths.workspace.display(),
        ));
        if store.session_root_id(session)? != session {
            prompt.push_str(&format!(
                "\nWork in the subfolder named {session} inside the shared chat scratchpad: {}",
                paths.chat.join(session).display(),
            ));
        }
        prompt.push_str(CLOSE);
        Ok(prompt)
    }));
}
