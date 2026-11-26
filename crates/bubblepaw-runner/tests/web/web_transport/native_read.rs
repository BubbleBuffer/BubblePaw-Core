use super::*;

#[tokio::test]
async fn actual_http_runs_native_tool_and_projects_history_without_archive_writes() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("fixture.txt"),
        "Native fixture contents",
    )
    .unwrap();
    let provider = ScriptedProvider::with_steps(
        vec![ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "read-fixture".into(),
                name: "js".into(),
                arguments: json!({"code":"const r=await tools.read({path:'fixture.txt'}); text(r.content);"}),
            }],
        }],
        "The native file was read.",
    );
    let mut native = None;
    let server = Server::start_building(
        Arc::new(provider),
        |store| {
            let mut tools = ToolRegistry::new();
            let processes =
                register_native_tools(&mut tools, workspace.path(), true, store).unwrap();
            let artifacts = Arc::new(
                ArtifactStore::new(workspace.path(), workspace.path().join("artifacts")).unwrap(),
            );
            let repl = Arc::new(CodeMode::new(workspace.path().to_path_buf(), artifacts).unwrap());
            repl.register(&mut tools).unwrap();
            native = Some((processes, repl));
            tools
        },
        WebServices::default(),
        bubblepaw_core::ExecutionMode::Js,
    )
    .await;
    let (processes, repl) = native.unwrap();
    let id = server.create().await;
    let (status, run) = server
        .post(
            &format!("/api/sessions/{id}/run"),
            json!({"text":"Read the fixture"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["status"], "running");
    assert!(run.get("text").is_none());
    server.harness.wait_for_idle(&id).await.unwrap();
    let before = server.harness.store().storage_stats().unwrap();
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(view["session"]["status"], "completed");
    assert_eq!(view["session"]["message_count"], 4);
    assert_eq!(view["messages"][2]["role"], "tool");
    let tool_text = view["messages"][2]["content"][0]["text"].to_string();
    assert!(tool_text.contains("Native fixture contents"));
    let reply = &view["messages"][3]["content"][0]["text"];
    assert_eq!(reply, "The native file was read.");
    assert!(view["runs"][0].get("text").is_none());
    assert!(view["active_run_id"].is_null());
    assert_eq!(view["live_paragraphs"], json!([]));
    for _ in 0..5 {
        server.get("/api/sessions").await;
        server.get(&format!("/api/sessions/{id}")).await;
    }
    let after = server.harness.store().storage_stats().unwrap();
    assert_eq!(before.transaction_count, after.transaction_count);
    assert_eq!(before.message_count, after.message_count);
    assert_eq!(before.wal_bytes, after.wal_bytes);
    let (status, fork) = server
        .post(
            &format!("/api/sessions/{id}/fork"),
            json!({"title":"Forked fixture"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let fork_view = server
        .get(&format!("/api/sessions/{}", fork["id"].as_str().unwrap()))
        .await;
    assert_eq!(fork_view["messages"].as_array().unwrap().len(), 4);
    assert_eq!(fork["parent_session_id"], id);
    let config = server.get("/api/config").await;
    assert_eq!(config["provider"], "demo");
    assert_eq!(config["session_limit"], 64);
    assert!(config.get("credentials").is_none());
    // No serving database was written alongside the canonical SQLite archive.
    let files: Vec<_> = std::fs::read_dir(server.directory.path())
        .unwrap()
        .map(|file| file.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !files
            .iter()
            .any(|file| file.contains("serving") || file.contains("projection"))
    );
    processes.shutdown().await;
    repl.shutdown().await;
    server.stop().await;
}
