//! Live reload checks: the root access sentence follows the read-only setting, and an
//! interrupted refresh does not acknowledge an unapplied edit. Split from live_config.rs.
use super::*;

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
