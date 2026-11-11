use super::*;

/// docs/web-contract.md: `Last-Event-ID` replays the logged events the client
/// missed, then goes live without duplicates; an unknown id gets `reset`.
#[tokio::test]
async fn sse_resume_with_last_event_id_replays_missed_events() {
    let server = Server::start(
        Arc::new(ScriptedProvider::new("Scripted answer.")),
        ToolRegistry::new(),
    )
    .await;
    let id = server.create().await;
    for text in ["first", "second"] {
        let (status, _) = server
            .post(&format!("/api/sessions/{id}/run"), json!({"text":text}))
            .await;
        assert_eq!(status, StatusCode::OK);
        server.harness.wait_for_idle(&id).await.unwrap();
    }
    let last = server.harness.last_sequence();
    assert!(last >= 6, "two runs logged at least six events");
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(view["seq"], last);
    let missed_from = 3;
    let response = server
        .client
        .get(format!("{}/api/events", server.base))
        .header("Last-Event-ID", missed_from.to_string())
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut ids = Vec::new();
    while ids.last() != Some(&last) {
        let frame = next_sse(&mut stream, &mut buffer).await;
        assert!(!frame.contains("event: reset"), "{frame}");
        if let Some(sequence) = frame_id(&frame) {
            ids.push(sequence);
        }
    }
    assert_eq!(ids, (missed_from + 1..=last).collect::<Vec<_>>());
    // Live after the replay, with the client recorded as origin.
    let (status, _) = server
        .post(
            &format!("/api/sessions/{id}/run"),
            json!({"text":"third","client":{"id":"phone","name":"Phone"}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut durable = Vec::new();
    loop {
        let frame = next_sse(&mut stream, &mut buffer).await;
        let data = frame_data(&frame);
        if data["durable"] == true {
            durable.push(frame_id(&frame).unwrap());
        }
        if data["type"] == "run_started" {
            assert_eq!(data["origin"]["id"], "phone");
        }
        if data["type"] == "session_settled" {
            break;
        }
    }
    assert_eq!(durable[0], last + 1, "no duplicate or gap after the replay");
    assert!(durable.windows(2).all(|pair| pair[1] == pair[0] + 1));
    let view = server.get(&format!("/api/sessions/{id}")).await;
    let third_input = view["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user" && message["content"] == "third")
        .unwrap();
    assert_eq!(third_input["origin"], "typed");
    // An id the log cannot answer from resets.
    let response = server
        .client
        .get(format!("{}/api/events", server.base))
        .header("Last-Event-ID", "999999")
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    assert!(
        next_sse(&mut stream, &mut String::new())
            .await
            .contains("event: reset")
    );
    server.stop().await;
}
