//! The idle completion notice stays durable without chunk transactions.
use super::*;

#[tokio::test]
async fn idle_completion_notice_is_durable_once_and_chatty_output_has_no_chunk_transactions() {
    use bubblepaw_core::{Harness, HarnessConfig, MessageOrigin, RunOptions, Store};
    use bubblepaw_runner::{artifacts::ArtifactStore, codemode::CodeMode};
    let fixture = Fixture::new(ExecConfig {
        max_output_bytes: 65_536,
        ..ExecConfig::default()
    });
    let store = Store::open_data_dir(fixture.data.path()).unwrap();
    fixture.processes.set_store(store.clone());
    let session = store.create_session("chatty process").unwrap();
    let mut registry = fixture.registry.clone();
    let repl = Arc::new(
        CodeMode::new(
            fixture._workspace.path().to_owned(),
            Arc::new(
                ArtifactStore::new(
                    fixture._workspace.path(),
                    fixture._workspace.path().join("artifacts"),
                )
                .unwrap(),
            ),
        )
        .unwrap(),
    );
    repl.register(&mut registry).unwrap();
    #[cfg(windows)]
    let command = windows_command::powershell_file(
        fixture._workspace.path(),
        "idle-chatty.ps1",
        "Start-Sleep -Seconds 1; [Console]::Out.Write(('a' * 2000000)); [Console]::Error.Write(('b' * 2000000))",
    );
    #[cfg(not(windows))]
    let command = r"sleep 1; head -c 2000000 /dev/zero | tr '\000' a; head -c 2000000 /dev/zero | tr '\000' b >&2";
    let provider = Arc::new(NoticeProvider {
        code: format!(
            "const job = await tools.exec({}); return {{handle:job.handle,status:job.status}};",
            json!({"command":command,"background":true})
        ),
        requests: Default::default(),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        registry.clone(),
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let run = harness
        .prompt(&session.id, "launch background job", RunOptions::default())
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    let jobs = fixture.processes.jobs(&session.id).unwrap();
    let handle = jobs["jobs"][0]["handle"].as_str().unwrap().to_owned();
    assert_eq!(jobs["jobs"][0]["status"], "running");
    let before = store.storage_stats().unwrap();
    let chat_path = fixture
        .data
        .path()
        .join("chats")
        .join(format!("{}.db", session.id));
    let chat_before = chat_measure(&chat_path);
    let done = call(
        &registry,
        &session.id,
        "wait",
        json!({"handle":handle,"timeoutMs":60000}),
    )
    .await;
    assert_eq!(done["status"], "exited");
    tokio::time::timeout(Duration::from_secs(30), async {
        while !store.pending_effects(&session.id).unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let after = store.storage_stats().unwrap();
    let chat_after = chat_measure(&chat_path);
    assert!(
        chat_before.1 >= 1,
        "starting an exec must keep an effect boundary"
    );
    assert_eq!(
        chat_after.0, chat_before.0,
        "pipe chunks must not become chat rows"
    );
    assert_eq!(
        chat_after.1, chat_before.1,
        "pipe chunks must not become effect rows"
    );
    assert_eq!(
        after.message_count, before.message_count,
        "output chunks must not become messages"
    );
    assert!(
        after
            .transaction_count
            .saturating_sub(before.transaction_count)
            <= 3,
        "only the effect completion boundary may write while idle"
    );
    assert!(after.database_bytes.saturating_sub(before.database_bytes) <= 128 * 1024);
    assert!(after.wal_bytes.saturating_sub(before.wal_bytes) <= 256 * 1024);
    eprintln!(
        "chatty job 4000000 bytes: message rows {}->{}, transactions {}->{}, SQLite bytes {}->{}, WAL bytes {}->{}",
        before.message_count,
        after.message_count,
        before.transaction_count,
        after.transaction_count,
        before.database_bytes,
        after.database_bytes,
        before.wal_bytes,
        after.wal_bytes
    );
    eprintln!(
        "chat file: messages {}->{}, effects {}->{}, bytes {}->{}, WAL bytes {}->{}",
        chat_before.0,
        chat_after.0,
        chat_before.1,
        chat_after.1,
        chat_before.2,
        chat_after.2,
        chat_before.3,
        chat_after.3
    );
    assert_eq!(spool_size(&fixture.data.path().join("jobs")), 65_536);
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        2,
        "completion must not wake an idle session"
    );
    assert!(
        !store
            .messages(&session.id)
            .unwrap()
            .iter()
            .any(|message| message.text().starts_with("<harness "))
    );
    assert_eq!(
        harness
            .prompt(&session.id, "check finished job", RunOptions::default())
            .await
            .unwrap()
            .status,
        "completed"
    );
    let messages = store.messages(&session.id).unwrap();
    let notices = messages
        .iter()
        .filter(|message| message.text().starts_with("<harness "))
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].origin, Some(MessageOrigin::Generated));
    assert!(notices[0].text().contains(&handle));
    assert!(
        notices[0]
            .text()
            .contains("stdout 2000000 bytes, stderr 2000000 bytes")
    );
    assert!(
        provider
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .iter()
            .any(|message| message.starts_with("<harness ") && message.contains(&handle))
    );
    assert_eq!(
        harness
            .prompt(&session.id, "check again", RunOptions::default())
            .await
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(
        store
            .messages(&session.id)
            .unwrap()
            .iter()
            .filter(|message| message.text().starts_with("<harness "))
            .count(),
        1
    );
    call(&registry, &session.id, "poll", json!({"handle":handle})).await;
    assert_eq!(spool_size(&fixture.data.path().join("jobs")), 0);
    repl.shutdown().await;
    fixture.processes.shutdown().await;
}
