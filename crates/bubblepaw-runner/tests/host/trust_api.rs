//! Workspace trust through the host API: an untrusted workspace's skipped project config reaches
//! the host as a session event, and a grant makes the next session apply it. Scripted only.
use bubblepaw_core::{Store, ToolRegistry};
use bubblepaw_mcp::McpHost;
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions},
    host_api::{HostCtx, Profile, dispatch},
    live_config::LiveConfig,
    provider::LiveProviderConfig,
    register_native_tools_with_exec,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const USER: &str = "[model]\nmain='demo/first'\n[approvals]\nconfirm='allow'\n[roles]\nsummary='main'\nchapter='main'\ndecider='main'\napprover='main'\n";

async fn call(ctx: &HostCtx, method: &str, params: Value) -> Value {
    dispatch(ctx, Profile::Full, method, params)
        .await
        .unwrap_or_else(|error| panic!("{method}: {error}"))
}

fn trust_events(events: &Value) -> Vec<&Value> {
    events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "host_state_changed" && event["key"] == "workspace_trust")
        .collect()
}

#[tokio::test]
async fn untrusted_workspace_emits_the_event_and_a_grant_applies_the_config_to_the_next_session() {
    let root = tempfile::tempdir().unwrap();
    let (workspace, home) = (root.path().join("workspace"), root.path().join("home"));
    std::fs::create_dir_all(workspace.join(".bubblepaw")).unwrap();
    std::fs::create_dir_all(home.join(".bubblepaw")).unwrap();
    std::fs::write(home.join(".bubblepaw/config.toml"), USER).unwrap();
    std::fs::write(
        workspace.join(".bubblepaw/config.toml"),
        "[agents]\nwake_debounce_ms=77\n",
    )
    .unwrap();
    let effective = EffectiveConfig::load(LoadOptions {
        workspace: workspace.clone(),
        home: Some(home),
        custom_file: None,
        env: vec![],
        cli: vec![],
    })
    .unwrap();
    let settings = effective.settings().unwrap();
    let store = Store::open(root.path().join("archive.db")).unwrap();
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools_with_exec(
        &mut tools,
        root.path(),
        false,
        settings.exec.clone(),
        root.path().join("state"),
    )
    .unwrap();
    let mcp = Arc::new(McpHost::start(&settings.mcp, root.path()).await);
    let providers = Arc::new(LiveProviderConfig::new(effective.clone()).unwrap());
    let live = LiveConfig::new(
        effective,
        providers.clone(),
        mcp.clone(),
        processes.clone(),
        store.clone(),
    )
    .unwrap();
    let mut ctx = HostCtx::for_store(store.clone(), workspace.clone());
    ctx.live_config = Some(live);
    ctx.provider_config = Some(providers.clone());

    let first = call(&ctx, "session.open", json!({})).await;
    let first = first["id"].as_str().unwrap();
    let events = call(&ctx, "events.after", json!({"session_id": first})).await;
    let notices = trust_events(&events);
    assert_eq!(notices.len(), 1, "{events}");
    let value = &notices[0]["value"];
    assert_eq!(value["status"], "untrusted");
    assert_eq!(value["sections"], json!(["agents"]));
    assert!(
        value["config_file"]
            .as_str()
            .unwrap()
            .ends_with("config.toml")
    );
    assert!(value["workspace"].as_str().unwrap().ends_with("workspace"));
    assert_ne!(providers.snapshot().settings.agents.wake_debounce_ms, 77);

    let granted = call(&ctx, "workspace_trust.trust", json!({"path": workspace})).await;
    assert_eq!(granted["reload"]["applied"], true, "{granted}");
    let second = call(&ctx, "session.open", json!({})).await;
    let second = second["id"].as_str().unwrap();
    assert_eq!(providers.snapshot().settings.agents.wake_debounce_ms, 77);
    assert!(
        store
            .host_state(second, "workspace_trust")
            .unwrap()
            .is_none()
    );
    let listed = call(&ctx, "workspace_trust.list", json!({})).await;
    assert_eq!(listed["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(listed["workspace"]["status"], "trusted");

    processes.shutdown().await;
    mcp.shutdown(Duration::from_secs(2)).await;
}
