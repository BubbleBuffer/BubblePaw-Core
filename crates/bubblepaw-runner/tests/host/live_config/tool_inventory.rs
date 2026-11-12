use super::*;

#[tokio::test]
async fn reload_tool_refreshes_next_round_and_diff_is_one_durable_message() {
    let fixture = Fixture::new(&config("")).await;
    fixture.turn("warm").await;
    let before = fixture.store.storage_stats().unwrap();
    for _ in 0..12 {
        let report = fixture.live.check().await;
        assert!(report.changed.is_empty());
        assert!(report.errors.is_empty());
    }
    assert!(fixture.live.reload(None).await.changed.is_empty());
    let idle = fixture.store.storage_stats().unwrap();
    assert_eq!(idle.transaction_count, before.transaction_count);
    assert_eq!(idle.wal_bytes, before.wal_bytes);
    assert_eq!(idle.message_count, before.message_count);
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    let edited = config(&format!(
        "[mcp.servers.late]\ncommand={command}\napproval='none'\n"
    ));
    // The edit happens inside a model tool round after the turn-start stat.
    let code = format!(
        "await tools.write({}); const r=await tools.reload({{}}); if(!r.applied) throw Error(JSON.stringify(r)); return r;",
        json!({"path":"config.toml","content":edited})
    );
    fixture.code("edit and reload", &code);
    fixture.turn("edit and reload").await;
    {
        let requests = fixture.script.requests.lock().unwrap();
        let last = requests.last().unwrap();
        let frozen = serde_json::to_vec(&requests[0].tools).unwrap();
        for request in requests.iter() {
            assert_eq!(serde_json::to_vec(&request.tools).unwrap(), frozen);
            assert_eq!(request.system_prompt, requests[0].system_prompt);
        }
        assert!(!last.tools[0].description.contains("mcp__late__echo"));
        let notice = last
            .messages
            .iter()
            .find(|message| {
                message
                    .content
                    .contains("<harness kind=\"tools_reloaded\">")
            })
            .expect("reload appends a separate inventory notice");
        let delta: serde_json::Value = serde_json::from_str(
            notice
                .content
                .lines()
                .find(|line| line.starts_with('{'))
                .unwrap(),
        )
        .unwrap();
        assert!(
            delta["added"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "mcp__late__echo"
                    && tool["deferred"] == true
                    && tool["server"] == "late")
        );
        assert!(!notice.content.contains("input_schema"));
        assert!(
            last.messages
                .iter()
                .any(|m| m.content.contains("Config changed:")
                    && m.content.contains("+ mcp__late__echo"))
        );
    }
    let notes = fixture.notices();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].len() < 4096);
    let after = fixture.store.storage_stats().unwrap();
    fixture.turn("unchanged").await;
    assert_eq!(fixture.notices(), notes);
    let second = fixture.store.create_session("second session").unwrap();
    fixture
        .harness
        .prompt(&second.id, "another session", RunOptions::default())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .store
            .messages(&second.id)
            .unwrap()
            .iter()
            .filter(|m| m.content.contains("<harness kind=\"live_config:"))
            .count(),
        0,
        "new chats bind current configuration without replaying historical reload notices"
    );
    assert!(
        fixture
            .script
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .tools[0]
            .description
            .contains("mcp__late__echo"),
        "new chats bind the current registry declarations"
    );
    let no_change = fixture.store.storage_stats().unwrap();
    for _ in 0..8 {
        assert!(fixture.live.reload(None).await.changed.is_empty());
    }
    let stable = fixture.store.storage_stats().unwrap();
    assert_eq!(stable.transaction_count, no_change.transaction_count);
    assert_eq!(stable.wal_bytes, no_change.wal_bytes);
    println!(
        "live config storage: unchanged refresh tx=0 WAL=0; durable diff {} bytes; changed turn total tx={}, WAL={} bytes",
        notes[0].len(),
        after.transaction_count - before.transaction_count,
        after.wal_bytes.saturating_sub(before.wal_bytes)
    );
    // Storage/context/cache edits are announced, but do not replace resources.
    fixture.write(&(edited + "[storage]\narchive_after_days=42\n[context]\ncompact_threshold_tokens=123456\n[web]\ncache_ttl_secs=17\n"));
    let report = fixture.live.reload(None).await;
    assert!(report.restart_needed);
    assert_ne!(fixture.providers.snapshot().settings.web.cache_ttl_secs, 17);
    fixture.shutdown().await;
}
