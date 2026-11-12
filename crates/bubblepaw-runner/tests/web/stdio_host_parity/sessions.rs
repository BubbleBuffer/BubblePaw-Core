use super::*;
use bubblepaw_core::{ContextStrategy, Store};
use std::collections::HashSet;

#[tokio::test]
async fn session_delete_returns_success_and_removes_session_from_list() {
    let root = fixture();
    let mut host = Host::start(root.path(), &[]);
    let id = host.open(json!({"title":"delete me"})).await;
    let listed = host.ok("session.list", json!({})).await;
    assert!(listed.as_array().unwrap().iter().any(|row| row["id"] == id));

    let deleted = host.ok("session.delete", json!({"session_id":id})).await;
    assert_eq!(deleted, json!({"deleted":id,"worktrees":[]}));
    let listed = host.ok("session.list", json!({})).await;
    assert!(listed.as_array().unwrap().iter().all(|row| row["id"] != id));
    host.stop().await;
}

#[tokio::test]
async fn session_delete_unknown_session_returns_error() {
    let root = fixture();
    let mut host = Host::start(root.path(), &[]);
    let response = host
        .call("session.delete", json!({"session_id":"unknown-session"}))
        .await;
    assert_eq!(response["error"]["code"], -32000);
    assert!(response["error"]["message"].is_string());
    assert!(response.get("result").is_none());
    host.stop().await;
}

#[tokio::test]
async fn session_list_paginates_visible_metadata_and_preserves_legacy_array() {
    let root = fixture();
    {
        let store = Store::open_data_dir(root.path().join("data")).unwrap();
        store
            .create_session_with_visibility("hidden", ContextStrategy::Lcm, None, true)
            .unwrap();
    }
    let mut host = Host::start(root.path(), &[]);
    for index in 0..5 {
        host.open(json!({"title":format!("session {index}")})).await;
    }
    let legacy = host.ok("session.list", json!({})).await;
    let legacy = legacy.as_array().unwrap();
    assert_eq!(legacy.len(), 5);
    assert!(
        legacy
            .iter()
            .all(|session| session["usage"].is_object() && session["hidden"] == false)
    );
    let mut expected = legacy.clone();
    expected.sort_by(|left, right| {
        right["created_at"]
            .as_str()
            .cmp(&left["created_at"].as_str())
            .then_with(|| right["id"].as_str().cmp(&left["id"].as_str()))
    });
    let mut cursor = Value::Null;
    let mut ids = Vec::new();
    loop {
        let page = host
            .ok("session.list", json!({"limit":2,"cursor":cursor}))
            .await;
        let sessions = page["sessions"].as_array().unwrap();
        assert!(sessions.len() <= 2);
        ids.extend(
            sessions
                .iter()
                .map(|session| session["id"].as_str().unwrap().to_owned()),
        );
        cursor = page["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());
    assert_eq!(
        ids,
        expected
            .iter()
            .map(|session| session["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    );
    let first = host.ok("session.list", json!({"limit":2})).await;
    let deleted = first["sessions"][1]["id"].as_str().unwrap();
    host.stop().await;
    // Exercise deletion and restart independently of the deletion RPC's reply.
    // The cursor names immutable metadata, not a live row.
    {
        let store = Store::open_data_dir(root.path().join("data")).unwrap();
        store.delete_session(deleted).unwrap();
    }
    let mut host = Host::start(root.path(), &[]);
    let next = host
        .ok("session.list", json!({"cursor":first["next_cursor"]}))
        .await;
    assert_eq!(next["sessions"].as_array().unwrap().len(), 3);
    assert!(next["next_cursor"].is_null());
    for input in [
        json!({"limit":0}),
        json!({"limit":-1}),
        json!({"limit":1001}),
        json!({"limit":1.5}),
        json!({"limit":null}),
        json!({"cursor":4}),
        json!({"cursor":""}),
        json!({"cursor":"not-a-cursor"}),
    ] {
        assert!(
            host.call("session.list", input)
                .await
                .get("error")
                .is_some()
        );
    }
    host.stop().await;
}
