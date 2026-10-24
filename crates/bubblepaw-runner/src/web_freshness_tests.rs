//! Cached serving views must match a cold rebuild after every kind of write on
//! a directory store, including writes that never touch a chat record.
use super::*;
use bubblepaw_core::{HarnessConfig, ToolRegistry};

fn state_for(store: Store) -> Arc<WebState> {
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(crate::provider::ScriptedProvider::new("fixture")),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let (events, _) = broadcast::channel(4);
    let control = SessionControl::new(harness.clone(), store.clone(), "fixture");
    Arc::new(WebState {
        harness,
        store,
        config: WebConfig {
            workspace: "fixture".into(),
            model: "fixture".into(),
            provider: "demo".into(),
            read_only: false,
        },
        port: 8788,
        serving: Mutex::new(ServingView::new().unwrap()),
        transient: Mutex::default(),
        events,
        refresh: Arc::new(Notify::new()),
        stop: CancellationToken::new(),
        services: WebServices::default(),
        control,
        wake: None,
        wake_host: Mutex::new(None),
    })
}

/// Read through the warm cache, then compare with a fresh serving view.
fn assert_fresh(state: &WebState, id: &str, step: &str) {
    let mut warm = state.serving.lock().unwrap();
    let mut cold = ServingView::new().unwrap();
    let cached = serde_json::to_value(refresh_list(state, &mut warm).unwrap()).unwrap();
    let rebuilt = serde_json::to_value(refresh_list(state, &mut cold).unwrap()).unwrap();
    assert_eq!(cached, rebuilt, "session list is stale after {step}");
    let (cached, _) = refresh_session(state, &mut warm, id).unwrap();
    let (rebuilt, _) = refresh_session(state, &mut cold, id).unwrap();
    assert_eq!(
        serde_json::to_value(cached).unwrap(),
        serde_json::to_value(rebuilt).unwrap(),
        "session view is stale after {step}"
    );
}

#[tokio::test]
async fn directory_store_views_follow_every_write() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_data_dir(root.path()).unwrap();
    let state = state_for(store.clone());
    let id = store.create_session("fresh").unwrap().id;
    assert_fresh(&state, &id, "create");

    let run = state
        .harness
        .prompt(&id, "hello", RunOptions::default())
        .await
        .unwrap();
    assert_fresh(&state, &id, "a completed run");

    store.rename_session(&id, "renamed").unwrap();
    assert_fresh(&state, &id, "rename");

    store
        .update_host_state(&id, &run.id, "question", |_| {
            Ok(json!({"status": "pending", "text": "?"}))
        })
        .unwrap();
    assert_fresh(&state, &id, "a pending question");

    store
        .create_session_with_parent("child", Default::default(), Some(&id))
        .unwrap();
    assert_fresh(&state, &id, "a child session");

    store.flush_serving().unwrap();
    assert_fresh(&state, &id, "a serving flush");

    let other = store.create_session("other").unwrap().id;
    store.delete_session(&other).unwrap();
    assert_fresh(&state, &id, "deleting another session");
}
