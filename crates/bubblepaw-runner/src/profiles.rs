//! Host-built agent types with restricted harnesses and explicit run policy.
use crate::{
    agent_handles::register_agent_handles_weak,
    agents::{AgentRegistry, AgentTools},
    codemode::CodeMode,
    config::Agents,
    net::WebConfig,
    provider::LiveProviderConfig,
    research::recursive_research_profile,
    tools::ProcessTools,
};
use anyhow::{Result, ensure};
use bubblepaw_core::{
    Harness, HarnessConfig, Provider, Store, ToolRegistry, derived_registry_resolver,
};
use std::{path::Path, sync::Arc};

#[derive(Clone)]
pub struct AgentProfile {
    pub name: String,
    pub description: String,
    pub harness: Arc<Harness>,
    pub model: Option<String>,
    /// Optional per-run override; None uses the harness's optional round limit.
    pub max_rounds: Option<usize>,
    pub approval: Option<bubblepaw_core::ApprovalPolicy>,
}

/// Add isolated per-session JS to a nonrecursive child tool set.
pub fn profile_harness(
    store: Store,
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    repl: &Arc<CodeMode>,
    config: HarnessConfig,
) -> Result<Arc<Harness>> {
    profile_harness_with_agents(store, provider, tools, repl, config, None)
}

/// Validate the base capabilities before deliberately adding host-authorized
/// nested child controls. The compatibility entry point never adds them.
pub(crate) fn profile_harness_with_agents(
    store: Store,
    provider: Arc<dyn Provider>,
    mut tools: ToolRegistry,
    repl: &Arc<CodeMode>,
    mut config: HarnessConfig,
    nested: Option<&Arc<AgentTools>>,
) -> Result<Arc<Harness>> {
    ensure!(
        tools.get("agent").is_none(),
        "child tools must not include agent"
    );
    ensure!(
        config.max_rounds != Some(0),
        "child max_rounds must be positive"
    );
    if let Some(nested) = nested {
        nested.register_weak(&mut tools)?;
        register_agent_handles_weak(&mut tools, nested)?;
    }
    repl.register(&mut tools)?;
    if let Some(source) = config.registry_resolver.take() {
        let weak_repl = Arc::downgrade(repl);
        let weak_nested = nested.map(Arc::downgrade);
        let controls_shown = ["agent", "wait", "cancel", "environments"]
            .into_iter()
            .filter(|name| tools.is_shown_always(name))
            .collect::<Vec<_>>();
        config.registry_resolver = Some(derived_registry_resolver(source, move |tools| {
            let models = tools
                .definitions()
                .into_iter()
                .filter(|tool| tool.name != "js")
                .map(|tool| tool.name)
                .collect::<Vec<_>>();
            let capabilities = tools
                .capabilities()
                .into_iter()
                .filter(|tool| tool.name != "environments")
                .map(|tool| tool.name)
                .collect::<Vec<_>>();
            // CodeMode owns both the JS model entrypoint and environment host
            // control. Rebuild them together over this child snapshot.
            let mut tools = tools
                .restricted(&models)
                .restricted_capabilities(&capabilities);
            if let Some(nested) = weak_nested.as_ref().and_then(std::sync::Weak::upgrade) {
                nested
                    .register_weak(&mut tools)
                    .expect("valid child controls");
                register_agent_handles_weak(&mut tools, &nested).expect("valid child handles");
            }
            if let Some(repl) = weak_repl.upgrade() {
                repl.register(&mut tools).expect("valid child JS tool");
            }
            tools.show_always(controls_shown.iter().copied());
            tools
        }));
    }
    Ok(Arc::new(Harness::new(store, provider, tools, config)))
}

/// The standard workspace profiles, inheriting any explicit harness round limit.
pub fn workspace_profiles(
    store: Store,
    provider: Arc<dyn Provider>,
    tools: &ToolRegistry,
    repl: &Arc<CodeMode>,
    config: &HarnessConfig,
    workspace: &Path,
    writable: bool,
) -> Result<Vec<AgentProfile>> {
    workspace_profiles_with_agents(
        store, provider, tools, repl, config, workspace, writable, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn workspace_profiles_with_agents(
    store: Store,
    provider: Arc<dyn Provider>,
    tools: &ToolRegistry,
    repl: &Arc<CodeMode>,
    config: &HarnessConfig,
    workspace: &Path,
    writable: bool,
    nested: Option<&Arc<AgentTools>>,
) -> Result<Vec<AgentProfile>> {
    let mut profiles = Vec::new();
    for (name, description, access, child_tools) in [
        (
            "read-only",
            "Read-only workspace child",
            "read-only (no commands or writes)",
            tools.read_only(),
        ),
        (
            "workspace",
            "Writable workspace child",
            "read/write",
            tools.clone(),
        ),
    ] {
        if name == "workspace" && !writable {
            continue;
        }
        let native = "Complete the assigned task using its supplied context and available tools. Put independent reads and checks together as multiple tool calls in one message. Keep dependent steps and mutations ordered. When submit_result is available, finish by calling it with the required result. Otherwise return your answer in your final response for the parent; presentation is the parent's responsibility.";
        let js = "Complete the assigned task using its supplied context and available tools. Batch independent reads/checks in JS. When submit_result is available, finish by calling it through tools.submit_result. Otherwise return your answer in your final response for the parent; presentation is the parent's responsibility. The js description defines the API and limits.";
        let prompt = |instructions| {
            format!(
                "Execution environment: OS={}, workspace={}, raw command shell={}, workspace access={access}. {instructions}",
                std::env::consts::OS,
                workspace.display(),
                crate::command_template::shell_name()
            )
        };
        let mut child_config = HarnessConfig {
            max_rounds: config.max_rounds,
            ..config.clone()
        };
        crate::execution_mode::configure_prompts(
            &mut child_config,
            store.clone(),
            prompt(native),
            prompt(js),
        );
        if let Some(source) = config.registry_resolver.clone() {
            let read_only = name == "read-only";
            let fallback = child_tools.clone();
            child_config.registry_resolver = Some(Arc::new(move || {
                let current = source().restricted(&[]);
                let current = if read_only {
                    current.read_only()
                } else {
                    current
                };
                let retained = current
                    .capabilities()
                    .into_iter()
                    .filter(|tool| {
                        !matches!(
                            tool.name.as_str(),
                            "agent" | "agent_send" | "agent_status" | "wait" | "cancel"
                        )
                    })
                    .map(|tool| tool.name)
                    .collect::<Vec<_>>();
                let mut current = current.restricted_capabilities(&retained);
                // Root wait/cancel route through a strong child manager. Keep
                // the original process fallback, then wrap with weak controls.
                for name in ["wait", "cancel"] {
                    if let Some(tool) = fallback.get_host(name) {
                        current.register_host(tool).expect("valid process fallback");
                    }
                }
                current
            }));
        }
        profiles.push(AgentProfile {
            name: name.into(),
            description: description.into(),
            model: None,
            approval: None,
            max_rounds: child_config.max_rounds,
            harness: profile_harness_with_agents(
                store.clone(),
                provider.clone(),
                child_tools,
                repl,
                child_config,
                nested,
            )?,
        });
    }
    Ok(profiles)
}

/// Build the host's child manager from the configured limits.
///
/// Every child harness holds a weak handle to one shared manager. Depth is an
/// admission limit rather than a startup-built profile ceiling, so raising it
/// later makes deeper delegation available without rebuilding child harnesses.
#[allow(clippy::too_many_arguments)]
pub fn child_agent_host(
    store: Store,
    provider: Arc<dyn Provider>,
    tools: &ToolRegistry,
    repl: &Arc<CodeMode>,
    config: &HarnessConfig,
    workspace: &Path,
    writable: bool,
    web: Option<&WebConfig>,
    limits: Agents,
    registry: Arc<AgentRegistry>,
    processes: Arc<ProcessTools>,
) -> Result<Arc<AgentTools>> {
    child_agent_host_impl(
        store, provider, tools, repl, config, workspace, writable, web, limits, registry,
        processes, None,
    )
}

/// Host child profiles whose research tools follow the shared provider generation.
#[allow(clippy::too_many_arguments)]
pub fn child_agent_host_live(
    store: Store,
    provider: Arc<dyn Provider>,
    tools: &ToolRegistry,
    repl: &Arc<CodeMode>,
    config: &HarnessConfig,
    workspace: &Path,
    writable: bool,
    web: Option<&WebConfig>,
    limits: Agents,
    registry: Arc<AgentRegistry>,
    processes: Arc<ProcessTools>,
    live: Arc<LiveProviderConfig>,
) -> Result<Arc<AgentTools>> {
    child_agent_host_impl(
        store,
        provider,
        tools,
        repl,
        config,
        workspace,
        writable,
        web,
        limits,
        registry,
        processes,
        Some(live),
    )
}

#[allow(clippy::too_many_arguments)]
fn child_agent_host_impl(
    store: Store,
    provider: Arc<dyn Provider>,
    tools: &ToolRegistry,
    repl: &Arc<CodeMode>,
    config: &HarnessConfig,
    workspace: &Path,
    writable: bool,
    web: Option<&WebConfig>,
    limits: Agents,
    registry: Arc<AgentRegistry>,
    processes: Arc<ProcessTools>,
    live: Option<Arc<LiveProviderConfig>>,
) -> Result<Arc<AgentTools>> {
    let manager = AgentTools::new_recursive(
        store.clone(),
        limits,
        registry,
        Some(repl.clone()),
        Some(processes.clone()),
        |manager| {
            let mut child_config = config.clone();
            manager.configure_child_approval(&mut child_config);
            let mut profiles = workspace_profiles_with_agents(
                store.clone(),
                provider.clone(),
                tools,
                repl,
                &child_config,
                workspace,
                writable,
                Some(manager),
            )?;
            if let Some(live) = &live {
                let live = Arc::downgrade(live);
                manager.set_profile_availability(Arc::new(move |name| match name {
                    "research" => live
                        .upgrade()
                        .is_some_and(|live| live.snapshot().settings.web.enabled),
                    "workspace" => live
                        .upgrade()
                        .is_some_and(|live| !live.snapshot().settings.approvals.read_only),
                    _ => true,
                }));
            }
            if web.is_some() || live.is_some() {
                let mut default_web = WebConfig::default();
                if let Some(live) = &live {
                    let initial = live.snapshot();
                    default_web.cache_ttl_secs = initial.settings.web.cache_ttl_secs;
                    default_web.cache_max_bytes = initial.settings.web.cache_max_bytes;
                }
                let research_web = web.unwrap_or(&default_web);
                profiles.push(recursive_research_profile(
                    store.clone(),
                    provider.clone(),
                    tools,
                    repl,
                    &child_config,
                    research_web,
                    live.clone(),
                    manager,
                )?);
            }
            Ok(profiles)
        },
    )?;
    crate::child_workspaces::install(
        &manager, store, provider, tools, config, workspace, writable, live, processes,
    )?;
    Ok(manager)
}
