//! Workspace read/edit contract, explicit deadlines and session cancellation.
use super::*;

#[tokio::test]
async fn native_read_edit_and_sorted_list_contract() {
    let directory = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::in_memory().unwrap();
    let mut registry = ToolRegistry::new();
    let processes =
        checkpoint_write::native_pack(&mut registry, directory.path(), false, &store).unwrap();
    for name in ["z.txt", "b.txt", "a.txt"] {
        call(
            &store,
            &registry,
            "write",
            json!({"path":name,"content":"one\ntwo\nthree\n"}),
        )
        .await;
    }
    assert_eq!(
        call(&store, &registry, "list", json!({"limit":1})).await["entries"][0]["name"],
        "a.txt"
    );
    let read = call(
        &store,
        &registry,
        "read",
        json!({"path":"a.txt","startLine":2,"lineCount":1}),
    )
    .await;
    assert_eq!(read["content"], "two\n");
    assert_eq!(read["totalLines"], 3);
    assert_eq!(read["returnedLines"], 1);
    call(
        &store,
        &registry,
        "edit",
        json!({"path":"a.txt","old_string":"two","new_string":"changed"}),
    )
    .await;
    let error = checkpoint_write::invoke(
        &store,
        &registry,
        "edit",
        json!({"path":"a.txt","old_string":"two","new_string":"stale"}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("no exact match"), "{error}");
    processes.shutdown().await;
}

#[tokio::test]
async fn native_explicit_deadline_is_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::in_memory().unwrap();
    let mut registry = ToolRegistry::new();
    let processes =
        checkpoint_write::native_pack(&mut registry, directory.path(), false, &store).unwrap();
    let started = call(
        &store,
        &registry,
        "exec",
        json!({"command":SLEEP,"timeoutMs":10,"yieldMs":0}),
    )
    .await;
    assert_eq!(
        call(
            &store,
            &registry,
            "wait",
            json!({"handle":started["handle"]})
        )
        .await["status"],
        "timed_out"
    );
    processes.shutdown().await;
}

#[tokio::test]
async fn session_cancel_hook_stops_detached_processes_once() {
    let directory = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::in_memory().unwrap();
    let mut registry = ToolRegistry::new();
    let processes =
        checkpoint_write::native_pack(&mut registry, directory.path(), false, &store).unwrap();
    let started = call(
        &store,
        &registry,
        "exec",
        json!({"command":SLEEP,"yieldMs":0}),
    )
    .await;
    assert!(registry.get("process_run").is_none());
    assert_eq!(registry.get("exec").unwrap().cancel_session("alien"), 0);
    assert_eq!(registry.get("exec").unwrap().cancel_session("owner"), 1);
    assert_eq!(registry.get("exec").unwrap().cancel_session("owner"), 0);
    let done = call(
        &store,
        &registry,
        "wait",
        json!({"handle":started["handle"]}),
    )
    .await;
    assert_eq!(done["status"], "cancelled");
    processes.shutdown().await;
}
