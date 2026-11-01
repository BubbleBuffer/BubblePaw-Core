//! Section owners: a runtime edit reaches the subscribed component without a restart,
//! an invalid section is not published, and a startup-only key is reported as such.
use super::*;

#[path = "config_set.rs"]
mod config_set;
#[path = "config_set_keys.rs"]
mod config_set_keys;
#[path = "provider_baseline.rs"]
mod provider_baseline;
#[path = "sections/toolsets.rs"]
mod toolsets;

#[tokio::test]
async fn model_section_subscribers_see_a_runtime_edit() {
    let fixture = Fixture::new(&config("")).await;
    let mut generation = fixture.providers.subscribe();
    assert_eq!(
        generation.borrow_and_update().settings.main_model,
        "demo/first"
    );
    fixture.write(
        &(config("").replace("demo/first", "alternate/second")
            + "[providers.alternate]\nkind='scripted'\n"),
    );
    assert!(fixture.live.reload(None).await.applied);
    assert!(generation.has_changed().unwrap());
    assert_eq!(generation.borrow().settings.main_model, "alternate/second");
    fixture.shutdown().await;
}

#[tokio::test]
async fn context_section_subscribers_see_live_keys_only() {
    let fixture = Fixture::new(&config("")).await;
    let mut context = fixture.providers.hub().context();
    context.borrow_and_update();
    fixture.write(&config(
        "[context]\nwindow_tokens=64000\nauto_summarize=false\n",
    ));
    let report = fixture.live.reload(None).await;
    assert!(report.applied && !report.restart_needed, "{report:?}");
    assert!(context.has_changed().unwrap());
    let live = context.borrow_and_update().clone();
    assert_eq!(live.window_tokens, Some(64000));
    assert!(!live.auto_summarize);
    // A startup-only context key is reported and leaves the section untouched.
    fixture.write(&config(
        "[context]\nwindow_tokens=64000\nauto_summarize=false\nstrategy='classical'\n",
    ));
    let report = fixture.live.reload(None).await;
    assert_eq!(report.restart_keys, ["context.strategy"]);
    assert!(!context.has_changed().unwrap());
    fixture.shutdown().await;
}

#[tokio::test]
async fn mcp_section_is_validated_before_it_is_published() {
    let fixture = Fixture::new(&config("")).await;
    let mut section = fixture.providers.hub().mcp();
    section.borrow_and_update();
    fixture.write(&config(
        "[mcp.servers.later]\ncommand='unused'\nenabled=false\n",
    ));
    assert!(fixture.live.reload(None).await.applied);
    assert!(section.has_changed().unwrap());
    assert!(section.borrow_and_update().servers.contains_key("later"));
    // An enabled server with neither command nor url is rejected; the section stays.
    fixture.write(&config("[mcp.servers.broken]\nenabled=true\n"));
    let report = fixture.live.reload(None).await;
    assert!(!report.applied, "{report:?}");
    assert!(!section.has_changed().unwrap());
    assert!(!section.borrow().servers.contains_key("broken"));
    fixture.shutdown().await;
}

#[tokio::test]
async fn storage_changes_are_reported_as_restart_only() {
    let fixture = Fixture::new(&config("")).await;
    let mut generation = fixture.providers.subscribe();
    generation.borrow_and_update();
    fixture.write(&config("[storage]\narchive_after_days=42\n"));
    let report = fixture.live.reload(None).await;
    assert!(report.restart_needed);
    assert_eq!(report.restart_keys, ["storage.archive_after_days"]);
    assert_eq!(
        fixture
            .providers
            .snapshot()
            .settings
            .storage
            .archive_after_days,
        30
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn exec_agents_approvals_web_and_skills_sections_follow_runtime_edits() {
    let fixture = Fixture::new(&config("")).await;
    let hub = fixture.providers.hub();
    let (mut exec, mut agents) = (hub.exec(), hub.agents());
    exec.borrow_and_update();
    agents.borrow_and_update();
    assert!(!hub.approvals().read_only && hub.skills().enabled);
    let edited = |extra: &str| {
        config("").replace("confirm='allow'", "confirm='deny'\nread_only=true")
            + "[exec]\nsync_timeout_secs=7\n[agents]\nmax_depth=2\n[skills]\nenabled=false\n\
               [web]\nuser_agent='probe'\n"
            + extra
    };
    fixture.write(&edited(""));
    let report = fixture.live.reload(None).await;
    assert!(report.applied && !report.restart_needed, "{report:?}");
    assert_eq!(exec.borrow_and_update().sync_timeout_secs, 7);
    assert_eq!(agents.borrow_and_update().max_depth, Some(2));
    assert!(hub.approvals().read_only);
    assert_eq!(
        fixture.live.approval().confirm,
        bubblepaw_core::ApprovalMode::Deny
    );
    assert_eq!(hub.web().user_agent, "probe");
    assert!(!hub.skills().enabled);
    // A startup-only web key is reported and leaves the section untouched.
    let before = hub.web().cache_ttl_secs;
    fixture.write(&edited("cache_ttl_secs=99\n"));
    let report = fixture.live.reload(None).await;
    assert_eq!(report.restart_keys, ["web.cache_ttl_secs"]);
    assert_eq!(hub.web().cache_ttl_secs, before);
    fixture.shutdown().await;
}
