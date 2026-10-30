//! Cached serving views must match a cold rebuild after every kind of write on
//! a directory store, including writes that never touch a chat record.
use super::{host::host_ctx, *};
use crate::host_api::{EmptyParams, SessionParams, views};
use bubblepaw_core::{HarnessConfig, RunOptions, ToolRegistry};
use serde_json::json;

fn state_for(store: Store) -> Arc<WebState> {
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(crate::provider::ScriptedProvider::new("fixture")),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let (events, _) = broadcast::channel(4);
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
        serving: Arc::new(Mutex::new(ServingView::new().unwrap())),
        transient: Mutex::default(),
        events,
        refresh: Arc::new(Notify::new()),
        stop: CancellationToken::new(),
        services: WebServices::default(),
        wake: None,
        wake_host: Mutex::new(None),
    })
}

/// Reads through the warm shared cache, then compares with a cold view: the same
/// host calls with a new cache over the same store.
fn assert_fresh(state: &WebState, id: &str, step: &str) {
    let warm = host_ctx(state);
    let mut cold = host_ctx(state);
    cold.views = Some(Arc::new(Mutex::new(ServingView::new().unwrap())));
    let (cached, _) = views::sidebar(&warm, EmptyParams {}).unwrap();
    let (rebuilt, _) = views::sidebar(&cold, EmptyParams {}).unwrap();
    assert_eq!(
        serde_json::to_value(cached).unwrap(),
        serde_json::to_value(rebuilt).unwrap(),
        "session list is stale after {step}"
    );
    let params = SessionParams {
        session_id: id.to_owned(),
    };
    let (cached, _) = views::session(&warm, params.clone()).unwrap();
    let (rebuilt, _) = views::session(&cold, params).unwrap();
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
