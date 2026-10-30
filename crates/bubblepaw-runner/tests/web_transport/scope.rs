use super::*;

#[tokio::test]
async fn workspace_guard_rejects_foreign_roots_without_running_them() {
    let server = Server::start(
        Arc::new(ScriptedProvider::new("fixture")),
        ToolRegistry::new(),
    )
    .await;
    let foreign = tempfile::tempdir().unwrap();
    let session = server
        .harness
        .store()
        .create_session_with_workspace("another workspace", foreign.path())
        .unwrap();
    for action in ["run", "resume", "cancel"] {
        let body = if action == "run" {
            json!({"text":"must not run here"})
        } else {
            json!({})
        };
        let (status, value) = server
            .post(&format!("/api/sessions/{}/{action}", session.id), body)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{value}");
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("another workspace")
        );
    }
    // Wire validation precedes dispatch, even for a foreign workspace.
    let (status, value) = server
        .post(
            &format!("/api/sessions/{}/cancel", session.id),
            json!({"text":"not a cancel parameter"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert!(value["error"].as_str().unwrap().contains("unknown field"));
    assert!(
        server
            .harness
            .store()
            .messages(&session.id)
            .unwrap()
            .is_empty()
    );
    server.stop().await;
}
