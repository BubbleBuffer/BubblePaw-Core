//! Live reload checks: the root access sentence follows the read-only setting, and an
//! interrupted refresh does not acknowledge an unapplied edit. Split from live_config.rs.
use super::*;

#[tokio::test]
async fn config_cursor_skips_prior_reloads_and_delivers_later_changes_once() {
    let fixture = Fixture::new(&config("")).await;
    fixture.turn("warm existing session").await;
    fixture.write(&config("[exec]\nsync_timeout_secs=7\n"));
    assert!(fixture.live.reload(None).await.applied);

    let unseen = fixture
        .store
        .create_session("unseen after reload")
        .unwrap()
        .id;
    assert!(fixture.live.take_reminders(&unseen).is_empty());
    let fresh = fixture
        .store
        .create_session("fresh after reload")
        .unwrap()
        .id;
    fixture.harness.request_template(&fresh).unwrap();
    assert!(fixture.live.take_reminders(&fresh).is_empty());
    let existing = fixture.live.take_reminders(&fixture.session);
    assert_eq!(existing.len(), 1);
    assert!(existing[0].text.contains("exec updated"));
    assert!(fixture.live.take_reminders(&fixture.session).is_empty());

    // Template resolution already established the fresh session, so a reload
    // before its first turn still belongs to it.
    fixture.write(&config("[exec]\nsync_timeout_secs=8\n"));
    assert!(fixture.live.reload(None).await.applied);
    let later = fixture.live.take_reminders(&fresh);
    assert_eq!(later.len(), 1);
    assert!(later[0].text.contains("exec updated"));
    assert!(fixture.live.take_reminders(&fresh).is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn child_config_cursor_starts_after_prior_reloads_and_keeps_later_changes_once() {
    let fixture = Fixture::new(&config("")).await;
    fixture.turn("warm parent").await;
    fixture.write(&config("[exec]\nsync_timeout_secs=7\n"));
    assert!(fixture.live.reload(None).await.applied);
    fixture.code("spawn config child", "const c=await tools.agent({task:'config child',profile:'read-only'}); const r=await wait({handle:c.handle}); if(r.status!=='completed') throw Error(JSON.stringify(r)); return 'done';");
    fixture.turn("spawn config child").await;
    let child = fixture
        .store
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|session| session.parent_session_id.as_deref() == Some(&fixture.session))
        .expect("child exists before next reload")
        .id;
    let child_notices = || {
        fixture
            .store
            .messages(&child)
            .unwrap()
            .into_iter()
            .filter(|message| {
                message.origin == Some(MessageOrigin::Generated)
                    && message.content.contains("Config changed:")
            })
            .collect::<Vec<_>>()
    };
    assert!(child_notices().is_empty());
    fixture.write(&config("[exec]\nsync_timeout_secs=8\n"));
    assert!(fixture.live.reload(None).await.applied);
    let follow_up = format!(
        "const c=await tools.agent_send({{sessionId:{},text:'continue config child'}}); const r=await wait({{handle:c.handle}}); if(r.status!=='completed') throw Error(JSON.stringify(r)); return 'done';",
        json!(child)
    );
    fixture.code("continue config child once", &follow_up);
    fixture.turn("continue config child once").await;
    assert_eq!(child_notices().len(), 1);
    assert!(child_notices()[0].content.contains("exec updated"));
    fixture.code("continue unchanged config child", &follow_up);
    fixture.turn("continue unchanged config child").await;
    assert_eq!(child_notices().len(), 1);
    fixture.shutdown().await;
}

#[tokio::test]
async fn root_access_line_follows_live_read_only_and_skips_children() {
    let fixture = Fixture::new(&config("")).await;
    let root_prompt = |fixture: &Fixture| {
        fixture
            .harness
            .request_template(&fixture.session)
            .unwrap()
            .system_prompt
    };
    assert!(root_prompt(&fixture).ends_with("Workspace access: read/write."));
    fixture.write(&config("").replace("confirm='allow'", "confirm='allow'\nread_only=true"));
    let report = fixture.live.reload(None).await;
    assert!(report.applied, "{:?}", report.errors);
    assert!(
        root_prompt(&fixture).ends_with("Workspace access: read-only (no commands or writes).")
    );
    fixture.code("spawn", "const c=await tools.agent({task:'child',profile:'read-only'}); const r=await wait({handle:c.handle}); if(r.status!=='completed') throw Error(JSON.stringify(r)); return 'done';");
    fixture.turn("spawn").await;
    let child = fixture
        .store
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|session| session.parent_session_id.is_some())
        .expect("child session")
        .id;
    let child_prompt = fixture
        .script
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| request.session_id == child)
        .expect("child must make a scripted provider request")
        .system_prompt
        .clone();
    assert!(child_prompt.contains("workspace access=read-only (no commands or writes)."));
    assert!(!child_prompt.contains("Workspace access: "));
    fixture.shutdown().await;
}

#[tokio::test]
async fn interrupted_refresh_does_not_acknowledge_an_unapplied_file_edit() {
    let fixture = Fixture::new(&config("")).await;
    fixture.turn("warm").await;
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    let pid = fixture._root.path().join("starting.pid");
    let pid_toml = toml::Value::String(pid.to_string_lossy().into_owned());
    fixture.write(&config(&format!("[mcp.servers.delayed]\ncommand={command}\nargs=['--initialize-delay-ms','500','--pid-file',{pid_toml}]\n")));
    let live = fixture.live.clone();
    let check = tokio::spawn(async move { live.check().await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !pid.is_file() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    check.abort();
    assert!(check.await.unwrap_err().is_cancelled());
    assert!(fixture.live.registry().get("mcp__delayed__echo").is_none());
    let report = fixture.live.check().await;
    assert!(report.applied, "{report:?}");
    assert!(fixture.live.registry().get("mcp__delayed__echo").is_some());
    fixture.turn("applied after interruption").await;
    assert_eq!(fixture.notices().len(), 1);
    fixture.shutdown().await;
}

#[tokio::test]
async fn live_context_and_config_reads_refresh_without_changing_frozen_context() {
    use bubblepaw_runner::host_api::{HostCtx, Profile, dispatch, views::ServingView};
    let initial = config("[context]\nwindow_tokens=100000\ncompact_threshold_tokens=90000\n");
    let fixture = Fixture::new(&initial).await;
    let mut ctx = HostCtx::new(
        fixture.harness.clone(),
        fixture.store.clone(),
        fixture._root.path(),
    );
    ctx.provider_config = Some(fixture.providers.clone());
    ctx.live_config = Some(fixture.live.clone());
    ctx.config =
        json!({"workspace":"fixture", "extra":"preserved", "model":"stale", "read_only":false});
    ctx.views = Some(Arc::new(Mutex::new(ServingView::new().unwrap())));
    let get = |method: &'static str, params| dispatch(&ctx, Profile::Full, method, params);
    let first = get("config.get", json!({})).await.unwrap();
    assert_eq!(first["model"], "demo/first");
    assert_eq!(first["context"]["window_tokens"], 100000);
    let first_view = get("session.view", json!({"session_id":fixture.session}))
        .await
        .unwrap();
    assert_eq!(
        first_view["view"]["usage"]["context"]["compaction_trigger_tokens"],
        74000
    );
    let revision = fixture.store.revision();
    let edited = config("[providers.alt]\nkind='scripted'\n[context]\nwindow_tokens=150000\ncompact_threshold_tokens=130000\nauto_summarize=false\nstrategy='classical'\nmemory_files=false\nretry_base_ms=1\n[context.lcm]\noutput_reserve_tokens=30000\n[context.classical]\nverbatim_user_tokens=12345\n")
        .replace("main='demo/first'", "main='alt/second'")
        .replace("confirm='allow'", "confirm='allow'\nread_only=true");
    fixture.write(&edited);
    let report = fixture.live.reload(None).await;
    assert!(report.applied, "{report:?}");
    assert!(report.restart_needed);
    for path in [
        "window_tokens",
        "compact_threshold_tokens",
        "auto_summarize",
    ] {
        assert!(
            report.changed.contains(&format!("context.{path} updated")),
            "{report:?}"
        );
        assert!(
            !report
                .changed
                .contains(&format!("context.{path} updated (restart needed)"))
        );
    }
    for path in [
        "strategy",
        "memory_files",
        "retry_base_ms",
        "lcm.output_reserve_tokens",
        "classical.verbatim_user_tokens",
    ] {
        assert!(
            report
                .changed
                .contains(&format!("context.{path} updated (restart needed)")),
            "{report:?}"
        );
    }
    let generation = fixture.providers.snapshot();
    assert_eq!(
        generation.settings.context_strategy,
        bubblepaw_core::ContextStrategy::Lcm
    );
    assert!(generation.settings.memory_files);
    assert_eq!(generation.settings.retry_base_ms, 500);
    assert_eq!(generation.settings.lcm.output_reserve_tokens, 16000);
    assert_eq!(generation.settings.verbatim_user_tokens, 20000);
    let current = get("config.get", json!({})).await.unwrap();
    assert_eq!(current["model"], "alt/second");
    assert_eq!(current["provider"], "alt");
    assert_eq!(current["read_only"], true);
    assert_eq!(current["extra"], "preserved");
    assert_eq!(
        current["context"],
        json!({"window_tokens":150000, "compact_threshold_tokens":130000, "auto_summarize":false})
    );
    // Reload changes no authoritative rows; cached views must still refresh.
    assert_eq!(fixture.store.revision(), revision);
    let cached = get("session.view", json!({"session_id":fixture.session}))
        .await
        .unwrap();
    let state = get("session.state", json!({"session_id":fixture.session}))
        .await
        .unwrap();
    let list = get("session.list", json!({"limit":1})).await.unwrap();
    for usage in [
        &cached["view"]["usage"],
        &state["usage"],
        &list["sessions"][0]["usage"],
    ] {
        assert_eq!(usage["context"]["model"], "alt/second");
        assert_eq!(usage["context"]["window_tokens"], 150000);
        assert_eq!(usage["context"]["compaction_trigger_tokens"], 114000);
        assert_eq!(usage["context"]["auto_compaction_enabled"], false);
    }
    // An invalid edit preserves both reports and request behavior.
    fixture.write(&edited.replace("window_tokens=150000", "window_tokens=16000"));
    assert!(!fixture.live.reload(None).await.applied);
    assert_eq!(get("config.get", json!({})).await.unwrap(), current);
    fixture.shutdown().await;
}

#[tokio::test]
async fn live_only_context_edits_need_no_restart_and_invalid_controls_are_atomic() {
    let fixture = Fixture::new(&config(
        "[context]\nwindow_tokens=100000\ncompact_threshold_tokens=90000\n",
    ))
    .await;
    fixture.write(&config(
        "[context]\nwindow_tokens=120000\ncompact_threshold_tokens=110000\nauto_summarize=false\n",
    ));
    let report = fixture.live.reload(None).await;
    assert!(report.applied, "{report:?}");
    assert!(!report.restart_needed);
    for controls in [
        "window_tokens=16000\ncompact_threshold_tokens=110000\n",
        "window_tokens=120000\ncompact_threshold_tokens=16000\n",
        // A changed disk reserve must not allow a value below the retained reserve.
        "window_tokens=12000\ncompact_threshold_tokens=110000\n[context.lcm]\noutput_reserve_tokens=1000\n",
    ] {
        fixture.write(&config(&format!("[context]\n{controls}")));
        let report = fixture.live.reload(None).await;
        assert!(!report.applied, "{report:?}");
        let generation = fixture.providers.snapshot();
        assert_eq!(generation.settings.context_window_tokens, Some(120000));
        assert_eq!(generation.settings.compact_threshold_tokens, 110000);
        assert!(!generation.settings.auto_summarize);
    }
    fixture.shutdown().await;
}
