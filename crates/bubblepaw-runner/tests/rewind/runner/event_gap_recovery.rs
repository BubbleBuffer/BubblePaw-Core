use super::*;

/// docs/session-api.md: a client that stops reading gets `events.gap` and
/// recovers every missed durable event with `events.after`.
#[tokio::test]
async fn stdio_events_after_round_trip_and_gap_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("archive.sqlite");
    std::fs::write(temp.path().join("burst.txt"), "x".repeat(1024)).unwrap();
    let mut child = tokio::process::Command::from(cli(temp.path(), &db))
        .args(["--set", "agent.execution_mode='js'", "--read-only", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(&mut stdin, 1, "session.open", json!({})).await;
    let session = response(&mut stdout, 1).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Snapshot plus follow: the state's seq is the starting point.
    send(
        &mut stdin,
        1000,
        "session.state",
        json!({"session_id":session}),
    )
    .await;
    let state = response(&mut stdout, 1000).await;
    let start = state["seq"].as_u64().unwrap();
    assert_eq!(state["streaming_text"], "");
    // One admitted script produces more than the 128-record output queue can
    // retain. Per-request replies cannot throttle this independent producer.
    send(
        &mut stdin,
        2000,
        "run.start",
        json!({
            "session_id":session,"text":"Flood events while the client stops reading",
            "client":{"id":"phone","name":"Phone"},
            "script":"for(let i=0;i<300;i++) await tools.read({path:'burst.txt'}); return true;"
        }),
    )
    .await;
    // Observe persisted terminal run status without draining stdout. A fixed sleep
    // can resume reading before a slow machine has filled the transport.
    let record = bubblepaw_core::Store::legacy_data_dir(&db).join("serving.db");
    let archive =
        rusqlite::Connection::open_with_flags(&record, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    archive.busy_timeout(Duration::from_secs(2)).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = archive.query_row(
                "SELECT status FROM runs WHERE session_id=?1",
                [&session],
                |row| row.get::<_, String>(0),
            );
            if let Ok(status) = status
                && status != "running"
            {
                assert_eq!(status, "completed");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let mut received = std::collections::BTreeSet::new();
    let mut gaps = Vec::new();
    let mut origins = 0;
    // Drain the retained prefix, then produce a new event to flush any pending
    // gap notification: the forwarder reports skipped records on its next event.
    for pass in 0..2 {
        if pass == 1 {
            send(&mut stdin, 2001, "run.start", json!({"session_id":session,"text":"Flush the pending gap","client":{"id":"phone","name":"Phone"},"script":"return true;"})).await;
        }
        while let Some(record) = try_next(&mut stdout, Duration::from_secs(1)).await {
            match record["method"].as_str() {
                Some("event") if record["params"]["durable"] == true => {
                    received.insert(record["params"]["sequence"].as_u64().unwrap());
                    if record["params"]["type"] == "run_started"
                        && record["params"]["origin"]["id"] == "phone"
                    {
                        origins += 1;
                    }
                }
                Some("events.gap") => {
                    assert_eq!(record["params"]["recover_with"], "events.after");
                    gaps.push(record["params"]["after"].as_u64().unwrap());
                }
                _ => {}
            }
        }
    }
    assert!(
        !gaps.is_empty(),
        "the flood produced a gap: received {} events",
        received.len()
    );
    // Recover: everything after each gap point, paging until caught up.
    let mut request = 3000;
    for after in gaps {
        let mut after = after;
        loop {
            send(
                &mut stdin,
                request,
                "events.after",
                json!({"after":after,"limit":100}),
            )
            .await;
            let page = response(&mut stdout, request).await;
            request += 1;
            assert_eq!(page["resync"], false);
            let events = page["events"].as_array().unwrap().clone();
            for event in &events {
                assert!(event["sequence"].as_u64().unwrap() > after);
                assert_eq!(event["durable"], true);
                assert_ne!(event["type"], "paragraph");
            }
            let Some(last) = events.last() else { break };
            after = last["sequence"].as_u64().unwrap();
            received.extend(events.iter().map(|e| e["sequence"].as_u64().unwrap()));
        }
    }
    send(&mut stdin, request, "events.after", json!({"after":start})).await;
    let page = response(&mut stdout, request).await;
    let last = page["last_seq"].as_u64().unwrap();
    let expected: std::collections::BTreeSet<u64> = (start + 1..=last).collect();
    assert_eq!(
        received
            .range(start + 1..)
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    let logged_origins = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "run_started" && e["origin"]["id"] == "phone")
        .count();
    assert!(logged_origins > 0 && origins <= 2);
    send(&mut stdin, request + 1, "shutdown", json!({})).await;
    response(&mut stdout, request + 1).await;
    drop(stdin);
}
