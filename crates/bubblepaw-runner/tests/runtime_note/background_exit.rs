use super::*;

#[tokio::test]
async fn background_exit_is_listed_until_its_result_is_polled() {
    let fixture = Fixture::new();
    #[cfg(windows)]
    let command = "Start-Sleep -Milliseconds 400; Write-Output private-command-output";
    #[cfg(not(windows))]
    let command = "sleep 0.4; echo private-command-output";
    let exec = fixture.registry.get("exec").unwrap();
    let started: Value = serde_json::from_str(
        &exec
            .invoke(
                fixture.context(),
                json!({"command":command,"background":true}),
            )
            .await
            .unwrap()
            .content,
    )
    .unwrap();
    let handle = started["handle"].as_str().unwrap().to_owned();
    let running = fixture.notes.live_note(&fixture.session);
    assert!(running.contains(&handle), "{running}");
    assert!(!running.contains("unread"), "{running}");
    // Let the job exit without polling it.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let exited = fixture.processes.continuation_status(&fixture.session)["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["status"] == "exited");
            if exited {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let unread = fixture.notes.live_note(&fixture.session);
    assert!(unread.contains(&handle), "{unread}");
    assert!(unread.contains("exited, unread, exit=0"), "{unread}");
    assert!(!unread.contains("private-command-output"));
    // Polling until the terminal status returns it to the model.
    let polled = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let value: Value = serde_json::from_str(
                &fixture
                    .registry
                    .get("poll")
                    .unwrap()
                    .invoke(fixture.context(), json!({"handle":handle,"yieldMs":200}))
                    .await
                    .unwrap()
                    .content,
            )
            .unwrap();
            if value["status"] != "running" {
                break value;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(polled["status"], "exited");
    let quiet = fixture.notes.live_note(&fixture.session);
    assert!(!quiet.contains(&handle), "{quiet}");
    assert!(quiet.contains("Runtime: no running commands, cells or agents."));
    fixture.shutdown().await;
}
