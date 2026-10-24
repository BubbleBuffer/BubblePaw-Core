//! Rebuild workspace-bound adapters for isolated child sessions.
use crate::{
    agents::AgentTools,
    artifacts::ArtifactStore,
    codemode::CodeMode,
    profiles::{AgentProfile, workspace_profiles_with_agents},
    tools::{ProcessTools, WorkspaceTools},
};
use anyhow::{Result, anyhow};
use bubblepaw_core::{HarnessConfig, Provider, Store, ToolRegistry, derived_registry_resolver};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(crate) struct ChildRuntime {
    pub profile: AgentProfile,
    pub repl: Arc<CodeMode>,
    pub processes: Arc<ProcessTools>,
}
pub(crate) type ChildRuntimeFactory =
    Arc<dyn Fn(&Path, &str) -> Result<ChildRuntime> + Send + Sync>;
pub(crate) struct WorkspaceHost {
    pub root: PathBuf,
    pub factory: ChildRuntimeFactory,
}

/// Keep the live non-workspace capabilities, then replace the native adapters.
/// The local process and JS managers must survive for the child's whole chat.
fn rebind(current: ToolRegistry, native: &ToolRegistry) -> ToolRegistry {
    let names = current
        .capabilities()
        .into_iter()
        .filter(|tool| {
            !matches!(
                tool.name.as_str(),
                "read"
                    | "list"
                    | "search"
                    | "edit"
                    | "write"
                    | "exec"
                    | "poll"
                    | "jobs"
                    | "wait"
                    | "cancel"
                    | "agent"
                    | "agent_send"
                    | "agent_status"
                    | "environments"
            )
        })
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    let mut tools = current.restricted(&[]).restricted_capabilities(&names);
    for definition in native.capabilities() {
        // A live read-only switch removes writes and commands here too.
        if current.get_host(&definition.name).is_some()
            && let Some(tool) = native.get_host(&definition.name)
        {
            tools
                .register_host(tool)
                .expect("valid isolated native tool");
            if native.is_shown_always(&definition.name) {
                tools.show_always([definition.name]);
            }
        }
    }
    tools
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install(
    manager: &Arc<AgentTools>,
    store: Store,
    provider: Arc<dyn Provider>,
    tools: &ToolRegistry,
    config: &HarnessConfig,
    workspace: &Path,
    writable: bool,
    live: Option<Arc<crate::provider::LiveProviderConfig>>,
    root_processes: Arc<ProcessTools>,
) -> Result<()> {
    let weak_manager = Arc::downgrade(manager);
    let base_tools = tools.clone();
    let config = config.clone();
    let root = workspace.to_owned();
    let workspaces = Mutex::new(HashMap::<PathBuf, WorkspaceTools>::new());
    let factory: ChildRuntimeFactory = Arc::new(move |workspace, profile_name| {
        let manager = weak_manager
            .upgrade()
            .ok_or_else(|| anyhow!("child host was released"))?;
        let data = store
            .data_dir()
            .ok_or_else(|| anyhow!("child worktrees require a directory store"))?;
        let native_workspace = {
            let mut cache = workspaces
                .lock()
                .map_err(|_| anyhow!("workspace cache poisoned"))?;
            if let Some(tools) = cache.get(workspace) {
                tools.clone()
            } else {
                let tools = root_processes.workspace_for(workspace)?;
                cache.insert(workspace.to_owned(), tools.clone());
                tools
            }
        };
        let mut native = ToolRegistry::new();
        native_workspace.register(&mut native, false)?;
        let processes = Arc::new(root_processes.for_workspace(native_workspace));
        processes.set_store(store.clone());
        processes.set_wake(manager.wake_coordinator());
        processes.register_wait(&mut native)?;
        processes.register(&mut native)?;
        native.show_always(crate::tools::ALWAYS_SHOWN_TOOLS.iter().copied());
        native.show_always(["jobs"]);
        let artifacts = Arc::new(ArtifactStore::new(workspace, data.join("artifacts"))?);
        let repl = Arc::new(CodeMode::new(workspace.to_owned(), artifacts)?);
        let mut child_config = config.clone();
        manager.configure_child_approval(&mut child_config);
        let current = child_config
            .registry_resolver
            .as_ref()
            .map(|source| source())
            .unwrap_or_else(|| base_tools.clone());
        let bound = rebind(current, &native);
        if let Some(source) = child_config.registry_resolver.take() {
            let native = native.clone();
            let processes = Arc::downgrade(&processes);
            let live = live.clone();
            child_config.registry_resolver =
                Some(derived_registry_resolver(source, move |current| {
                    if let (Some(processes), Some(live)) = (processes.upgrade(), live.as_ref()) {
                        processes.apply_config(live.snapshot().settings.exec.clone());
                    }
                    rebind(current, &native)
                }));
        }
        let mut profiles = workspace_profiles_with_agents(
            store.clone(),
            provider.clone(),
            &bound,
            &repl,
            &child_config,
            workspace,
            writable,
            Some(&manager),
        )?;
        if profile_name == "research" {
            let default_web = crate::net::WebConfig::default();
            let profile = crate::research::recursive_research_profile(
                store.clone(),
                provider.clone(),
                &bound,
                &repl,
                &child_config,
                &default_web,
                live.clone(),
                &manager,
            )?;
            profiles.push(profile);
        }
        let profile = profiles
            .into_iter()
            .find(|profile| profile.name == profile_name)
            .ok_or_else(|| {
                anyhow!("profile {profile_name} is unavailable in an isolated workspace")
            })?;
        Ok(ChildRuntime {
            profile,
            repl,
            processes,
        })
    });
    manager.set_workspace_host(WorkspaceHost { root, factory })
}
