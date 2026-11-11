use super::*;

#[tokio::test]
async fn concurrent_delegations_attach_to_one_host_and_checks_stay_in_it() {
    let lab = Lab::new();
    let data = lab.data();
    let first_ws = lab.dir("first");
    let second_ws = lab.dir("second");
    std::fs::write(second_ws.join("done.txt"), "done").unwrap();
    let first_file = lab.path("first.md");
    std::fs::write(&first_file, assignment(CHECK_TWICE, 1)).unwrap();
    let second_file = lab.path("second.md");
    std::fs::write(&second_file, assignment(CHECK, 0)).unwrap();
    // Every worker step submits this result; checks run only in the host.
    let report = json!({
        "status": "done",
        "summary": "worker says done",
        "checks_claimed": [],
        "open_questions": [],
    })
    .to_string();

    // The first delegation is slowed, so the second attaches while the host is busy.
    let mut first_cmd = lab.command(
        &data,
        &first_ws,
        "fake/code-model",
        &[],
        &["delegate", first_file.to_str().unwrap()],
    );
    extension_env(&mut first_cmd, "BP_FAKE_REPLY", &report);
    extension_env(&mut first_cmd, "BP_FAKE_REPLY_AS_RESULT", "1");
    extension_env(&mut first_cmd, "BP_FAKE_REPLY_DELAY_MS", "1500");
    let mut first = Proc::spawn(lab.root(), "first", first_cmd);
    first
        .until("the host's first worker step", || !lab.records().is_empty())
        .await;
    host_json(&data).expect("host.json published");

    let mut second_cmd = lab.command(
        &data,
        &second_ws,
        "fake/code-model",
        &[],
        &["delegate", second_file.to_str().unwrap()],
    );
    extension_env(&mut second_cmd, "BP_FAKE_REPLY", &report);
    extension_env(&mut second_cmd, "BP_FAKE_REPLY_AS_RESULT", "1");
    let mut second = Proc::spawn(lab.root(), "second", second_cmd);

    assert!(
        second.finish().await.success(),
        "stdout: {}; stderr: {}",
        second.stdout(),
        second.stderr()
    );
    assert!(
        first.finish().await.success(),
        "stdout: {}; stderr: {}",
        first.stdout(),
        first.stderr()
    );
    for (name, proc) in [("first", &first), ("second", &second)] {
        let last: Value = serde_json::from_str(proc.stdout().lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "delegation", "{name}: {}", proc.stdout());
        assert_eq!(last["status"], "accepted", "{name}: {}", proc.stdout());
    }

    // The host ran the first assignment's check twice: once failing, once after rework.
    let checks = std::fs::read_to_string(first_ws.join("checks.log")).unwrap();
    assert_eq!(
        checks.lines().count(),
        2,
        "checks ran in the host, once per attempt"
    );

    // One ledger row per attempt: two for the reworked delegation, one for the other.
    let ledger = rusqlite::Connection::open(data.join("delegations.db")).unwrap();
    let mut per_session: Vec<i64> = ledger
        .prepare("SELECT COUNT(*) FROM delegation_attempts GROUP BY session_id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    per_session.sort_unstable();
    assert_eq!(per_session, vec![1, 2]);

    let report_output = lab
        .command(
            &data,
            &first_ws,
            "fake/code-model",
            &[],
            &["delegations", "report"],
        )
        .output()
        .await
        .unwrap();
    assert!(report_output.status.success());
    let summary: Value = serde_json::from_slice(&report_output.stdout).unwrap();
    let accepted: u64 = summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["accepted"].as_u64().unwrap())
        .sum();
    assert_eq!(accepted, 2, "{summary}");
}
