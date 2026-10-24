use super::*;
use crate::{
    Approval, ApprovalPolicy, CancellationToken, Decision, HarnessConfig, ModelRequest,
    ModelResponse, Provider, RunOptions, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition,
    ToolRegistry, ToolResult,
};
use async_trait::async_trait;
use std::{
    sync::{Arc, atomic::AtomicUsize, atomic::Ordering},
    time::Duration,
};

/// Streams two paragraphs per round and calls `echo` for `rounds` rounds.
struct Talk {
    rounds: usize,
    round: AtomicUsize,
}
fn talk(rounds: usize) -> Arc<Talk> {
    Arc::new(Talk {
        rounds,
        round: AtomicUsize::new(0),
    })
}
#[async_trait]
impl Provider for Talk {
    async fn generate(
        &self,
        _: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        on_text("First paragraph.\n\n");
        tokio::time::sleep(Duration::from_millis(1)).await;
        on_text("Second paragraph.\n\n");
        let text = "First paragraph.\n\nSecond paragraph.".to_string();
        if round < self.rounds {
            return Ok(ModelResponse {
                text,
                tool_calls: vec![ToolCall {
                    id: format!("call-{round}"),
                    name: "echo".into(),
                    arguments: serde_json::json!({}),
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text,
            ..Default::default()
        })
    }
}

/// Waits for cancellation.
struct Hang;
#[async_trait]
impl Provider for Hang {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        cancellation.cancelled().await;
        anyhow::bail!("cancelled")
    }
}

/// Calls `write` once, then finishes.
struct Write(AtomicUsize);
#[async_trait]
impl Provider for Write {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: "w1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}

struct Echo {
    name: &'static str,
    approval: Approval,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "echo".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: self.approval == Approval::None,
            approval: self.approval,
        }
    }
    async fn invoke(&self, _: ToolContext, _: serde_json::Value) -> Result<ToolResult> {
        tokio::time::sleep(Duration::from_millis(1)).await;
        Ok(ToolResult::text("echoed"))
    }
}
fn tools(name: &'static str, approval: Approval) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Echo { name, approval })).unwrap();
    tools
}
fn harness(store: &Store, provider: Arc<dyn Provider>, config: HarnessConfig) -> Harness {
    Harness::new(
        store.clone(),
        provider,
        tools("echo", Approval::None),
        HarnessConfig {
            event_capacity: 4096,
            ..config
        },
    )
}
fn drain(events: &mut broadcast::Receiver<HarnessEvent>) -> Vec<HarnessEvent> {
    std::iter::from_fn(|| events.try_recv().ok()).collect()
}
fn durable(events: &[HarnessEvent]) -> Vec<u64> {
    events
        .iter()
        .filter(|e| e.durable)
        .map(|e| e.sequence)
        .collect()
}
fn client(id: &str) -> Option<ClientInfo> {
    Some(ClientInfo {
        id: id.into(),
        name: Some(format!("{id} device")),
    })
}
async fn until<F: Fn(&HarnessEvent) -> bool>(
    events: &mut broadcast::Receiver<HarnessEvent>,
    matches: F,
) -> HarnessEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches(&event) {
                return event;
            }
        }
    })
    .await
    .expect("event")
}

#[tokio::test]
async fn sequence_stays_monotonic_across_a_store_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let (session, first_last) = {
        let store = Store::open(&path)?;
        let session = store.create_session("s")?;
        let agent = harness(&store, talk(1), HarnessConfig::default());
        agent
            .prompt(&session.id, "go", RunOptions::default())
            .await?;
        let last = agent.last_sequence();
        assert!(last > 0);
        (session, last)
    };
    let store = Store::open(&path)?;
    let agent = harness(&store, talk(0), HarnessConfig::default());
    let restarted_floor = agent.last_sequence();
    assert!(restarted_floor > first_last);
    assert!(!agent.can_resume_from(first_last));
    let mut events = agent.subscribe();
    agent
        .prompt(&session.id, "again", RunOptions::default())
        .await?;
    let live = drain(&mut events);
    assert_eq!(durable(&live)[0], restarted_floor + 1);
    let logged: Vec<u64> = agent
        .events_after(0, None, 1000)?
        .iter()
        .map(|e| e.sequence)
        .collect();
    let mut expected: Vec<_> = (1..=first_last).collect();
    expected.extend(durable(&live));
    assert_eq!(logged, expected);
    Ok(())
}

#[tokio::test]
async fn events_after_returns_missed_durable_events_and_paragraphs_stay_live() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("s")?;
    let agent = harness(&store, talk(2), HarnessConfig::default());
    let mut events = agent.subscribe();
    agent
        .prompt(&session.id, "go", RunOptions::default())
        .await?;
    let live = drain(&mut events);
    let paragraphs = live
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Paragraph { .. }))
        .count();
    assert_eq!(paragraphs, 6);
    let mut last_durable = 0;
    for event in &live {
        if event.durable {
            assert_eq!(
                event.sequence,
                last_durable + 1,
                "durable sequence is contiguous"
            );
            last_durable = event.sequence;
        } else {
            assert!(matches!(
                event.kind,
                EventKind::Paragraph { .. }
                    | EventKind::TextDelta { .. }
                    | EventKind::ModelRequestStarted { .. }
            ));
            assert_eq!(
                event.sequence, last_durable,
                "live carries the last durable"
            );
        }
    }
    let logged = agent.events_after(0, Some(&session.id), 1000)?;
    let expected: Vec<&HarnessEvent> = live.iter().filter(|e| e.durable).collect();
    assert_eq!(logged.len(), expected.len());
    for (logged, live) in logged.iter().zip(&expected) {
        assert_eq!(logged.sequence, live.sequence);
        assert!(logged.durable);
        assert_eq!(
            serde_json::to_value(&logged.kind)?,
            serde_json::to_value(&live.kind)?
        );
    }
    assert!(
        !logged
            .iter()
            .any(|e| matches!(e.kind, EventKind::Paragraph { .. }))
    );
    // A client that saw up to `k` gets exactly the rest, in order.
    let k = expected[3].sequence;
    let missed: Vec<u64> = agent
        .events_after(k, Some(&session.id), 1000)?
        .iter()
        .map(|e| e.sequence)
        .collect();
    assert_eq!(missed, (k + 1..=last_durable).collect::<Vec<_>>());
    assert_eq!(agent.events_after(0, None, 2)?.len(), 2);
    assert!(agent.events_after(0, Some("other"), 1000)?.is_empty());
    assert!(agent.events_after(last_durable, None, 1000)?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_seq_and_follow_have_no_gap_or_duplicate_under_a_running_run() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("s")?;
    let agent = harness(&store, talk(12), HarnessConfig::default());
    let run = agent.start(&session.id, "go", RunOptions::default())?;
    let mut readers = Vec::new();
    loop {
        let events = agent.subscribe();
        let (messages, position) =
            agent.snapshot_with(&session.id, || store.messages(&session.id))?;
        readers.push((events, messages, position));
        if store.run(&run.id)?.status != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_micros(300)).await;
    }
    agent.wait_for_idle(&session.id).await?;
    let last = agent.last_sequence();
    let log = agent.events_after(0, None, 1000)?;
    assert!(readers.len() > 3, "the run raced several snapshots");
    for (mut events, messages, position) in readers {
        let followed: Vec<u64> = drain(&mut events)
            .iter()
            .filter(|e| e.durable && e.sequence > position.seq)
            .map(|e| e.sequence)
            .collect();
        assert_eq!(followed, (position.seq + 1..=last).collect::<Vec<_>>());
        // Every event the snapshot claims to reflect has its state in it.
        for event in log.iter().filter(|e| e.sequence <= position.seq) {
            if let EventKind::ToolFinished { call_id, .. } = &event.kind {
                assert!(
                    messages
                        .iter()
                        .any(|m| m.tool_call_id.as_deref() == Some(call_id))
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn streaming_text_holds_the_reply_until_it_is_stored() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("s")?;
    let agent = harness(&store, Arc::new(Hang), HarnessConfig::default());
    let mut events = agent.subscribe();
    let run = agent.start(&session.id, "go", RunOptions::default())?;
    until(&mut events, |e| {
        matches!(e.kind, EventKind::RunStarted { .. })
    })
    .await;
    agent.emit(
        &session.id,
        &run.id,
        EventKind::Paragraph {
            text: "One.".into(),
        },
    );
    agent.emit(
        &session.id,
        &run.id,
        EventKind::Paragraph {
            text: "Two.".into(),
        },
    );
    let ((), position) = agent.snapshot_with(&session.id, || Ok(()))?;
    assert_eq!(position.streaming_text, "One.\n\nTwo.");
    assert_eq!(position.seq, agent.last_sequence());
    assert_eq!(position.live_seen, 2);
    agent.log().settle_text(&session.id, || Ok(()))?;
    let ((), position) = agent.snapshot_with(&session.id, || Ok(()))?;
    assert_eq!(position.streaming_text, "");
    agent.emit(
        &session.id,
        &run.id,
        EventKind::FollowupQueued { text: "x".into() },
    );
    let ((), position) = agent.snapshot_with(&session.id, || Ok(()))?;
    assert_eq!(position.live_seen, 0, "a durable event starts a new count");
    agent.cancel(&session.id);
    agent.wait_for_idle(&session.id).await?;
    Ok(())
}

#[tokio::test]
async fn child_agent_approvals_reach_the_parent_stream_and_the_log() -> Result<()> {
    let store = Store::in_memory()?;
    let parent = harness(&store, talk(0), HarnessConfig::default());
    let child = Harness::new(
        store.clone(),
        Arc::new(Write(AtomicUsize::new(0))),
        tools("write", Approval::Confirm),
        HarnessConfig {
            approval: ApprovalPolicy::default(),
            ..Default::default()
        },
    );
    let child_session = store.create_session("child")?;
    let mut events = parent.subscribe();
    child.start(&child_session.id, "write it", RunOptions::default())?;
    let requested = until(&mut events, |e| {
        matches!(e.kind, EventKind::ApprovalRequested { .. })
    })
    .await;
    assert_eq!(requested.session_id, child_session.id);
    assert!(requested.durable);
    let EventKind::ApprovalRequested { approval } = &requested.kind else {
        unreachable!()
    };
    let logged = parent.events_after(requested.sequence - 1, None, 1)?;
    assert_eq!(logged[0].sequence, requested.sequence);
    assert!(matches!(
        logged[0].kind,
        EventKind::ApprovalRequested { .. }
    ));
    parent.decide(&approval.id, Decision::Approve)?;
    child.wait_for_idle(&child_session.id).await?;
    Ok(())
}

#[tokio::test]
async fn origin_is_recorded_for_start_steer_followup_cancel_and_decide() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("s")?;
    let agent = harness(&store, Arc::new(Hang), HarnessConfig::default());
    let mut events = agent.subscribe();
    let run = agent.start(
        &session.id,
        "go",
        RunOptions {
            client: client("desktop"),
            ..Default::default()
        },
    )?;
    let error = agent
        .start(&session.id, "again", RunOptions::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains(&run.id), "{error}");
    agent.steer_from(&session.id, "left", client("phone"))?;
    agent.followup_from(&session.id, "then", client("web"))?;
    assert!(agent.cancel_from(&session.id, client("phone")));
    agent.wait_for_idle(&session.id).await?;
    let live = drain(&mut events);
    let origin = |matches: fn(&EventKind) -> bool| {
        live.iter()
            .find(|e| matches(&e.kind))
            .and_then(|e| e.origin.clone())
            .map(|c| c.id)
    };
    assert_eq!(
        origin(|k| matches!(k, EventKind::RunStarted { .. })).as_deref(),
        Some("desktop")
    );
    assert_eq!(
        origin(|k| matches!(k, EventKind::SteeringAccepted { .. })).as_deref(),
        Some("phone")
    );
    assert_eq!(
        origin(|k| matches!(k, EventKind::FollowupQueued { .. })).as_deref(),
        Some("web")
    );
    assert_eq!(
        origin(|k| matches!(k, EventKind::RunFinished { status, .. } if status == "cancelled"))
            .as_deref(),
        Some("phone")
    );
    // The log keeps origin.
    let logged = agent.events_after(0, Some(&session.id), 1000)?;
    let steer = logged
        .iter()
        .find(|e| matches!(e.kind, EventKind::SteeringAccepted { .. }))
        .unwrap();
    assert_eq!(steer.origin, client("phone"));

    let approvals = Harness::new(
        store.clone(),
        Arc::new(Write(AtomicUsize::new(0))),
        tools("write", Approval::Confirm),
        HarnessConfig::default(),
    );
    let other = store.create_session("approve")?;
    approvals.start(&other.id, "write it", RunOptions::default())?;
    let requested = until(&mut events, |e| {
        matches!(e.kind, EventKind::ApprovalRequested { .. })
    })
    .await;
    let EventKind::ApprovalRequested { approval } = requested.kind else {
        unreachable!()
    };
    approvals.decide_from(&approval.id, Decision::Approve, client("phone"))?;
    let resolved = until(&mut events, |e| {
        matches!(e.kind, EventKind::ApprovalResolved { .. })
    })
    .await;
    assert_eq!(resolved.origin, client("phone"));
    approvals.wait_for_idle(&other.id).await?;
    let logged = approvals.events_after(resolved.sequence - 1, None, 1)?;
    assert_eq!(logged[0].origin, client("phone"));
    Ok(())
}

#[tokio::test]
async fn retention_prunes_old_events_and_session_deletion_deletes_its_events() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    {
        let store = Store::open(&path)?;
        let session = store.create_session("s")?;
        let agent = harness(&store, talk(1), HarnessConfig::default());
        agent
            .prompt(&session.id, "go", RunOptions::default())
            .await?;
        assert!(agent.last_sequence() > 4);
        store.write(|tx| {
            tx.execute(
                "UPDATE events SET created_at='2000-01-01T00:00:00+00:00' WHERE seq<=3",
                [],
            )?;
            Ok(())
        })?;
        // No retention configured: nothing is pruned.
        let kept = harness(&store, talk(0), HarnessConfig::default());
        assert_eq!(kept.events_after(0, None, 1000)?[0].sequence, 1);
        let pruning = harness(
            &store,
            talk(0),
            HarnessConfig {
                event_retention_days: Some(30),
                ..Default::default()
            },
        );
        assert_eq!(pruning.events_after(0, None, 1000)?[0].sequence, 4);
        assert_eq!(pruning.pruned_through(), 3);
        assert!(!pruning.can_resume_from(2));
        assert!(pruning.can_resume_from(3));
        assert!(!pruning.can_resume_from(pruning.last_sequence() + 1));
    }
    let store = Store::open(&path)?;
    let agent = harness(&store, talk(0), HarnessConfig::default());
    assert!(
        agent.pruned_through() >= 3,
        "the prune bound survives a reopen"
    );
    assert!(
        !agent.can_resume_from(3),
        "restart burns the prior sequence reservation"
    );

    let bare = store.create_session("bare")?;
    agent.emit(
        &bare.id,
        "r",
        EventKind::FollowupQueued { text: "x".into() },
    );
    assert_eq!(agent.events_after(0, Some(&bare.id), 1000)?.len(), 1);
    store.delete_session(&bare.id)?;
    assert!(agent.events_after(0, Some(&bare.id), 1000)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn state_and_journal_share_turn_transactions_and_wire_payloads() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("batched")?;
    let agent = harness(&store, talk(1), HarnessConfig::default());
    let mut events = agent.subscribe();
    let before = store.storage_stats()?.transaction_count;
    let run = agent
        .prompt(&session.id, "go", RunOptions::default())
        .await?;
    // Admission, combined assistant/dispatch, result and final checkpoint.
    // The in-memory journal shares record commits; directory stores add one
    // buffered projection commit, verified by storage_batching.
    assert_eq!(store.storage_stats()?.transaction_count - before, 4);
    let live: Vec<_> = drain(&mut events)
        .into_iter()
        .filter(|event| event.durable)
        .collect();
    let replay = agent.events_after(0, Some(&session.id), 1000)?;
    assert_eq!(serde_json::to_value(&live)?, serde_json::to_value(&replay)?);
    assert!(
        store.run(&run.id)?.text.is_none(),
        "run reads remain metadata-only"
    );
    assert_eq!(store.hydrate_run(store.run(&run.id)?)?.text, run.text);
    store.read(|conn| {
        let entries: Vec<String> = conn
            .prepare("SELECT data FROM events ORDER BY seq")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for data in entries {
            assert!(
                !data.contains("echoed"),
                "tool output belongs to the result message"
            );
            assert!(
                !data.contains("First paragraph"),
                "reply text belongs to its message"
            );
            assert!(
                !data.contains("arguments"),
                "tool args belong to the assistant message"
            );
        }
        let result_id: String = conn.query_row(
            "SELECT result_message_id FROM runs WHERE id=?1",
            [&run.id],
            |row| row.get(0),
        )?;
        assert_eq!(
            Store::read_message(conn, &session.id, &result_id)?.content,
            run.text.unwrap()
        );
        Ok(())
    })?;
    Ok(())
}

#[tokio::test]
async fn final_reply_and_terminal_events_use_one_checkpoint() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("final")?;
    let agent = harness(&store, talk(0), HarnessConfig::default());
    let before = store.storage_stats()?.transaction_count;
    agent
        .prompt(&session.id, "go", RunOptions::default())
        .await?;
    assert_eq!(store.storage_stats()?.transaction_count - before, 2);
    let (messages, position) = agent.snapshot_with(&session.id, || store.messages(&session.id))?;
    assert_eq!(messages.len(), 2);
    assert_eq!(position.streaming_text, "");
    assert!(matches!(
        agent.events_after(position.seq - 1, None, 1)?[0].kind,
        EventKind::SessionSettled {}
    ));
    Ok(())
}

#[tokio::test]
async fn failed_journal_append_rolls_back_state_and_does_not_advance_sequence() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("atomic")?;
    let agent = harness(&store, talk(0), HarnessConfig::default());
    let mut events = agent.subscribe();
    store.write(|tx| {
        tx.execute_batch("CREATE TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'journal failure'); END;")?;
        Ok(())
    })?;
    let error = agent
        .start(
            &session.id,
            "accepted only on commit",
            RunOptions::default(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("journal failure"));
    assert!(store.messages(&session.id)?.is_empty());
    assert!(store.runs(&session.id)?.is_empty());
    assert_eq!(agent.last_sequence(), 0);
    assert!(drain(&mut events).is_empty());
    Ok(())
}

#[tokio::test]
async fn legacy_full_events_and_new_references_replay_together() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("legacy")?;
    let old = HarnessEvent {
        sequence: 1,
        durable: true,
        origin: client("old"),
        session_id: session.id.clone(),
        run_id: "legacy-run".into(),
        created_at: now(),
        kind: EventKind::ToolFinished {
            call_id: "old-call".into(),
            name: "old-tool".into(),
            result: ToolResult::text("legacy bytes"),
        },
    };
    store.write(|tx| {
        tx.execute("INSERT INTO events(seq,session_id,run_id,created_at,kind,data) VALUES(?1,?2,?3,?4,?5,?6)",
            params![old.sequence as i64, old.session_id, old.run_id, old.created_at, "tool_finished", serde_json::to_string(&old)?])?;
        Ok(())
    })?;
    store.runtime().log.seed(1, 0);
    let agent = harness(&store, talk(0), HarnessConfig::default());
    agent
        .prompt(&session.id, "new", RunOptions::default())
        .await?;
    let replay = agent.events_after(0, Some(&session.id), 1000)?;
    assert_eq!(
        serde_json::to_value(&replay[0])?,
        serde_json::to_value(&old)?
    );
    assert_eq!(replay.last().unwrap().sequence, 4);
    Ok(())
}

#[tokio::test]
async fn consumed_input_references_replay_the_original_client_events() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("input refs")?;
    let agent = harness(&store, Arc::new(Hang), HarnessConfig::default());
    let mut all = agent.subscribe();
    agent.start(&session.id, "start", RunOptions::default())?;
    let steer = agent.steer_from(&session.id, "steer payload", client("steerer"))?;
    let followup = agent.followup_from(&session.id, "followup payload", client("follower"))?;
    let pending_events: Vec<_> = agent
        .events_after(0, Some(&session.id), 1000)?
        .into_iter()
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::SteeringAccepted { .. } | EventKind::FollowupQueued { .. }
            )
        })
        .collect();
    assert_eq!(pending_events.len(), 2);
    assert_eq!(
        store
            .queued_inputs(&session.id)?
            .into_iter()
            .filter(|input| input.state == "pending")
            .map(|input| input.text)
            .collect::<Vec<_>>(),
        vec![steer.text.clone(), followup.text.clone()]
    );
    store.read(|conn| {
        for input in [&steer, &followup] {
            let (text, content_blob): (String, String) = conn.query_row(
                "SELECT text,content_blob FROM inputs WHERE id=?1",
                [&input.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert!(text.is_empty(), "pending content belongs to tier 1");
            assert_eq!(
                String::from_utf8(Store::blob_get(conn, &content_blob)?)?,
                input.text
            );
        }
        Ok(())
    })?;
    agent.cancel(&session.id);
    agent.wait_for_idle(&session.id).await?;
    let resumed = harness(&store, talk(0), HarnessConfig::default());
    resumed.resume(&session.id, RunOptions::default()).await?;
    let consumed_events: Vec<_> = agent
        .events_after(0, Some(&session.id), 1000)?
        .into_iter()
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::SteeringAccepted { .. } | EventKind::FollowupQueued { .. }
            )
        })
        .collect();
    assert_eq!(
        serde_json::to_value(&pending_events)?,
        serde_json::to_value(consumed_events)?
    );
    let live: Vec<_> = drain(&mut all)
        .into_iter()
        .filter(|event| event.durable)
        .collect();
    assert_eq!(
        serde_json::to_value(live)?,
        serde_json::to_value(agent.events_after(0, Some(&session.id), 1000)?)?
    );
    store.read(|conn| {
        for input in [&steer, &followup] {
            let (text, message_id): (String, String) = conn.query_row(
                "SELECT text,message_id FROM inputs WHERE id=?1",
                [&input.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert!(text.is_empty());
            assert_eq!(
                Store::read_message(conn, &session.id, &message_id)?.content,
                input.text
            );
        }
        let data: String = conn.query_row(
            "SELECT group_concat(data) FROM events WHERE session_id=?1",
            [&session.id],
            |row| row.get(0),
        )?;
        assert!(!data.contains("steer payload"));
        assert!(!data.contains("followup payload"));
        Ok(())
    })?;
    Ok(())
}

#[tokio::test]
async fn resolved_approval_replay_preserves_the_requested_snapshot_without_arguments_copies()
-> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("approval refs")?;
    let agent = Harness::new(
        store.clone(),
        Arc::new(Write(AtomicUsize::new(0))),
        tools("write", Approval::Confirm),
        HarnessConfig::default(),
    );
    let mut control = agent.subscribe();
    let mut all = agent.subscribe();
    agent.start(&session.id, "write", RunOptions::default())?;
    let requested = until(&mut control, |event| {
        matches!(event.kind, EventKind::ApprovalRequested { .. })
    })
    .await;
    let EventKind::ApprovalRequested { approval } = requested.kind else {
        unreachable!()
    };
    agent.decide_from(
        &approval.id,
        Decision::Deny {
            reason: Some("unique denial reason".into()),
        },
        client("owner"),
    )?;
    agent.wait_for_idle(&session.id).await?;
    let live: Vec<_> = drain(&mut all)
        .into_iter()
        .filter(|event| event.durable)
        .collect();
    assert_eq!(
        serde_json::to_value(live)?,
        serde_json::to_value(agent.events_after(0, Some(&session.id), 1000)?)?
    );
    store.read(|conn| {
        let (arguments, reference): (String, String) = conn.query_row(
            "SELECT arguments,arguments_ref FROM approvals WHERE id=?1",
            [&approval.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert!(arguments.is_empty());
        assert!(reference.contains("message_id"));
        let rows: Vec<String> = conn
            .prepare(
                "SELECT data FROM events WHERE kind IN ('approval_requested','approval_resolved')",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|data| !data.contains("arguments") && !data.contains("unique denial reason"))
        );
        Ok(())
    })?;
    Ok(())
}

struct CountedTool(Arc<AtomicUsize>);
#[async_trait]
impl Tool for CountedTool {
    fn definition(&self) -> ToolDefinition {
        Echo {
            name: "echo",
            approval: Approval::None,
        }
        .definition()
    }
    async fn invoke(&self, _: ToolContext, _: serde_json::Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("completed external work"))
    }
}

async fn rejected_tool_journal_boundary(rejected_kind: &str, dispatched: bool) -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session(rejected_kind)?;
    let invoked = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(CountedTool(invoked.clone())))?;
    let agent = Harness::new(store.clone(), talk(1), registry, HarnessConfig::default());
    store.write(|tx| {
        tx.execute_batch(&format!("CREATE TRIGGER reject_boundary BEFORE INSERT ON events WHEN NEW.kind='{rejected_kind}' BEGIN SELECT RAISE(ABORT,'journal failure'); END;"))?;
        Ok(())
    })?;
    let run = agent
        .prompt(&session.id, "go", RunOptions::default())
        .await?;
    assert_eq!(run.status, "failed");
    assert_eq!(invoked.load(Ordering::SeqCst), usize::from(dispatched));
    let messages = store.messages(&session.id)?;
    if dispatched {
        assert_eq!(messages.len(), 3);
        assert!(messages[2].is_error);
        assert!(messages[2].content.contains("outcome unknown"));
        assert_eq!(store.pending_effects(&session.id)?[0]["state"], "unknown");
    } else {
        // In a single-file store, a rejected combined assistant/dispatch
        // transaction rolls back both rows before any external invocation.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, crate::Role::User);
        assert!(
            run.error
                .as_deref()
                .is_some_and(|error| error.contains("journal failure"))
        );
        assert!(store.pending_effects(&session.id)?.is_empty());
    }
    assert!(
        !agent
            .events_after(0, Some(&session.id), 1000)?
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolFinished { .. }))
    );
    agent.resume(&session.id, RunOptions::default()).await?;
    assert_eq!(
        invoked.load(Ordering::SeqCst),
        usize::from(dispatched),
        "resumption cannot replay an uncertain effect"
    );
    Ok(())
}

#[tokio::test]
async fn rejected_tool_started_journal_rolls_back_the_effect_before_dispatch() -> Result<()> {
    rejected_tool_journal_boundary("tool_started", false).await
}

#[tokio::test]
async fn rejected_tool_finished_journal_retains_an_unknown_effect_without_replay() -> Result<()> {
    rejected_tool_journal_boundary("tool_finished", true).await
}
