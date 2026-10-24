//! The ordinary restricted research child profile.
use crate::{
    agents::AgentTools,
    codemode::CodeMode,
    net::{
        WebConfig, register_research_sources_with_store, register_research_sources_with_store_live,
    },
    profiles::{AgentProfile, profile_harness_with_agents},
    provider::LiveProviderConfig,
};
use anyhow::Result;
use bubblepaw_core::{HarnessConfig, Provider, Store, ToolRegistry, derived_registry_resolver};
use std::sync::Arc;

pub const RESEARCH_PROMPT: &str = "Answer the assigned question using web_search, web_fetch and the available workspace reads. Put independent searches, fetches and workspace reads together as multiple tool calls in one message. Search, read the best sources and cross-check them. When submit_result is available, finish by calling it with the required result. Be brief. Say plainly when sources disagree or nothing was found. Cite returned stable source IDs and passage references such as s3#p12 next to supported claims. Never retype source URLs; source lookup resolves IDs to URLs. When no submit_result tool is available, return your answer in your final response for the parent. Continue the same research when the parent sends a follow-up.";
const JS_RESEARCH_PROMPT: &str = "Answer the assigned question using web_search, web_fetch and the available workspace reads through js. When submit_result is available, finish by calling it through tools.submit_result. Search, read the best sources and cross-check them. Be brief. Say plainly when sources disagree or nothing was found. Cite returned stable source IDs and passage references such as s3#p12 next to supported claims. Never retype source URLs; source lookup resolves IDs to URLs. When no submit_result tool is available, return your answer in your final response for the parent. Continue the same research when the parent sends a follow-up.";

/// Keep the profile's independent web budgets while following the live generation.
#[allow(clippy::too_many_arguments)]
pub fn research_profile_with_tools_live(
    store: Store,
    provider: Arc<dyn Provider>,
    host_tools: &ToolRegistry,
    repl: &Arc<CodeMode>,
    config: &HarnessConfig,
    web: &WebConfig,
    live: Arc<LiveProviderConfig>,
) -> Result<AgentProfile> {
    research_profile_with_tools_impl(
        store,
        provider,
        host_tools,
        repl,
        config,
        web,
        Some(live),
        None,
    )
}

/// Add the host's read-only workspace reads and live config reload capability.
/// Research has no commands, writes, MCP capabilities or nested child controls.
pub fn research_profile_with_tools(
    store: Store,
    provider: Arc<dyn Provider>,
    host_tools: &ToolRegistry,
    repl: &Arc<CodeMode>,
    config: &HarnessConfig,
    web: &WebConfig,
) -> Result<AgentProfile> {
    research_profile_with_tools_impl(store, provider, host_tools, repl, config, web, None, None)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn recursive_research_profile(
    store: Store,
    provider: Arc<dyn Provider>,
    host_tools: &ToolRegistry,
    repl: &Arc<CodeMode>,
    config: &HarnessConfig,
    web: &WebConfig,
    live: Option<Arc<LiveProviderConfig>>,
    nested: &Arc<AgentTools>,
) -> Result<AgentProfile> {
    research_profile_with_tools_impl(
        store,
        provider,
        host_tools,
        repl,
        config,
        web,
        live,
        Some(nested),
    )
}

#[allow(clippy::too_many_arguments)]
fn research_profile_with_tools_impl(
    store: Store,
    provider: Arc<dyn Provider>,
    host_tools: &ToolRegistry,
    repl: &Arc<CodeMode>,
    config: &HarnessConfig,
    web: &WebConfig,
    live: Option<Arc<LiveProviderConfig>>,
    nested: Option<&Arc<AgentTools>>,
) -> Result<AgentProfile> {
    let mut tools = ToolRegistry::new();
    for name in ["list", "read", "search", "reload", "question", "todo"] {
        if let Some(tool) = host_tools.get_host(name)
            && tool.definition().read_only
        {
            tools.register_host(tool)?;
            if host_tools.is_shown_always(name) {
                tools.show_always([name]);
            }
        }
    }
    if let Some(live) = live {
        register_research_sources_with_store_live(&mut tools, web, &store, live)?;
    } else {
        register_research_sources_with_store(&mut tools, web, &store)?;
    }
    let mut child_config = config.clone();
    crate::execution_mode::configure_prompts(
        &mut child_config,
        store.clone(),
        RESEARCH_PROMPT.into(),
        JS_RESEARCH_PROMPT.into(),
    );
    if let Some(source) = config.registry_resolver.clone() {
        let research_tools = tools.clone();
        child_config.registry_resolver = Some(derived_registry_resolver(source, move |current| {
            let mut tools = ToolRegistry::new();
            for name in ["list", "read", "search", "reload", "question", "todo"] {
                if let Some(tool) = current.get_host(name)
                    && tool.definition().read_only
                {
                    tools
                        .register_host(tool)
                        .expect("valid research workspace read");
                    if current.is_shown_always(name) {
                        tools.show_always([name]);
                    }
                }
            }
            // Research keeps independent budgets and both source tools even
            // when one or both tools are hidden from the parent.
            for name in ["web_search", "web_fetch"] {
                if let Some(tool) = research_tools.get_host(name) {
                    tools
                        .register_host(tool)
                        .expect("valid research source tool");
                    tools.show_always([name]);
                }
            }
            tools
        }));
    }
    let harness = profile_harness_with_agents(store, provider, tools, repl, child_config, nested)?;
    Ok(AgentProfile {
        name: "research".into(),
        description: "Search, read and cross-check sources in a reusable child session".into(),
        harness,
        model: None,
        max_rounds: config.max_rounds,
        approval: None,
    })
}
