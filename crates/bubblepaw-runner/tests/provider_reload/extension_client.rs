use super::*;

#[path = "../support/fixture_binary.rs"]
mod fixture_binary;

#[tokio::test]
async fn extension_clients_survive_publication_with_a_fresh_history_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let command = serde_json::to_string(&fixture_binary::resolve(
        env!("CARGO_BIN_EXE_bubblepaw-fake-extension"),
        "bubblepaw-fake-extension",
    ))
    .unwrap();
    let initial =
        format!("[model]\nmain='fixture/test'\n[extensions.fixture]\ncommand={command}\n");
    let live = LiveProviderConfig::new(load(&path, &initial)).unwrap();
    let mut step = request("fixture/test");
    step.session_id = "synthetic-session".into();
    step.history.epoch = "host-epoch".into();
    let original = generate(&live, step.clone())
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert_eq!(original["history"]["epoch"], "host-epoch");
    let changed = initial.replace(
        &format!("command={command}"),
        "command='this-edited-extension-needs-a-restart'",
    );
    {
        let _update = live.lock_update().unwrap();
        live.apply_prepared(live.prepare(load(&path, &changed)).unwrap());
    }
    let next = generate(&live, step.clone())
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert_eq!(original["pid"], next["pid"]);
    assert_ne!(next["history"]["epoch"], "host-epoch");
    let followup = generate(&live, step).await.unwrap().provider_state.unwrap();
    assert_eq!(next["history"]["epoch"], followup["history"]["epoch"]);
    assert_eq!(followup["step_count"], 3);
    assert!(
        live.snapshot()
            .effective
            .restart_needed(live.startup_config())
    );
    live.close_session("synthetic-session").await.unwrap();
}
