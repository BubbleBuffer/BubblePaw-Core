use super::*;
use bubblepaw_core::{ModelRequest, ModelResponse, Provider, QuotaWait, TextCallback};
use std::sync::atomic::AtomicUsize;

struct Probe(AtomicUsize);
#[async_trait]
impl Provider for Probe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert_eq!(request.model, "original/model");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse {
            tool_calls: vec![bubblepaw_core::ToolCall {
                id: "recovered-result".into(),
                name: "submit_result".into(),
                arguments: json!({"answer":"recovered"}),
            }],
            ..Default::default()
        })
    }
}

fn restored_wait(
    path: &std::path::Path,
    profile: &str,
    worker: bool,
) -> Result<(Store, String, String)> {
    let store = Store::open_data_dir(path)?;
    let parent = store.create_session("parent")?;
    let child = store.create_session_with_visibility(
        "child",
        Default::default(),
        Some(&parent.id),
        worker,
    )?;
    let binding = Binding {
        profile: profile.into(),
        model: Some("original/model".into()),
        max_rounds: Some(3),
        approval: Some(ApprovalPolicy {
            deny: vec!["child-deny".into()],
            ..Default::default()
        }),
        worker,
    };
    store.store_attachment(
        &child.id,
        BINDING_ID,
        &serde_json::to_string(&binding)?,
        b"",
    )?;
    let parent_binding = Binding {
        profile: "workspace".into(),
        model: None,
        max_rounds: None,
        approval: Some(ApprovalPolicy {
            deny: vec!["parent-deny".into()],
            ..Default::default()
        }),
        worker: false,
    };
    store.store_attachment(
        &parent.id,
        BINDING_ID,
        &serde_json::to_string(&parent_binding)?,
        b"",
    )?;
    store.append_batch(&[bubblepaw_core::Message {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: child.id.clone(),
        role: Role::User,
        origin: Some(MessageOrigin::ChildTask),
        content: "original task".into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-20T12:00:00Z".into(),
    }])?;
    let run = Run {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: child.id.clone(),
        model: "original/model".into(),
        status: "running".into(),
        text: None,
        error: None,
        started_at: "2026-10-20T12:00:00Z".into(),
        ended_at: None,
        usage: None,
    };
    let conn = rusqlite::Connection::open(store.chat_path(&child.id)?.unwrap())?;
    conn.execute(
        "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",
        rusqlite::params![run.id, child.id, serde_json::to_string(&run)?],
    )?;
    save_controls(
        &store,
        &child.id,
        &run.id,
        &Controls {
            output_schema: Some(JsonSchema::new(
                "answer",
                json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
            )?),
            max_tool_calls: Some(4),
            timeout: Some(60_000),
        },
    )?;
    store.put_quota_wait(&QuotaWait {
        session_id: child.id.clone(),
        run_id: run.id.clone(),
        account: "work".into(),
        window: "weekly".into(),
        resume_at: "2026-10-20T12:01:00Z".into(),
        reason: "fixture".into(),
    })?;
    drop(conn);
    drop(store);
    Ok((Store::open_data_dir(path)?, child.id, run.id))
}

fn manager(store: &Store, probe: Arc<Probe>) -> Result<AgentTools> {
    AgentTools::from_profiles(
        store.clone(),
        vec![AgentProfile {
            name: "read-only".into(),
            description: "original restricted profile".into(),
            harness: Arc::new(Harness::new(
                store.clone(),
                probe,
                ToolRegistry::new(),
                HarnessConfig::default(),
            )),
            model: None,
            max_rounds: Some(3),
            approval: None,
        }],
    )
    .map(|manager| {
        manager.with_limits(Agents {
            auto_wake: false,
            ..Default::default()
        })
    })
}

#[tokio::test]
async fn quota_recovery_uses_original_child_profile_schema_and_ancestral_restrictions() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let (store, session, old_run) = restored_wait(dir.path(), "read-only", false)?;
    let probe = Arc::new(Probe(AtomicUsize::new(0)));
    let manager = manager(&store, probe.clone())?;
    let inputs_before = store
        .messages(&session)?
        .into_iter()
        .filter(|message| {
            message.role == Role::User && message.origin != Some(MessageOrigin::Generated)
        })
        .count();
    let (run, harness) = manager.resume_quota_wait(
        &store.quota_wait(&session)?.unwrap(),
        ApprovalPolicy {
            deny: vec!["host-deny".into()],
            ..Default::default()
        },
    )?;
    assert_ne!(run.id, old_run);
    let child = manager.child(
        store
            .session(&session)?
            .parent_session_id
            .as_deref()
            .unwrap(),
        &session,
    )?;
    {
        let turn = child.turn.lock().unwrap();
        for name in ["host-deny", "parent-deny", "child-deny"] {
            assert!(turn.approval.deny.contains(&name.to_owned()));
        }
        assert_eq!(turn.controls.max_tool_calls, Some(4));
        assert_eq!(turn.controls.timeout, Some(60_000));
        assert!(turn.controls.output_schema.is_some());
    }
    assert_eq!(harness.wait_for_idle(&session).await?.status, "completed");
    await_child(&child, None).await?;
    assert_eq!(probe.0.load(Ordering::SeqCst), 1);
    assert!(store.quota_wait(&session)?.is_none());
    assert_eq!(
        store
            .messages(&session)?
            .into_iter()
            .filter(|message| message.role == Role::User
                && message.origin != Some(MessageOrigin::Generated))
            .count(),
        inputs_before
    );
    Ok(())
}

#[tokio::test]
async fn missing_dynamic_profile_cannot_use_the_inspection_fallback_to_resume() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (store, session, old_run) = restored_wait(dir.path(), "internal-worker-gone", true)?;
    let probe = Arc::new(Probe(AtomicUsize::new(0)));
    let manager = manager(&store, probe.clone())?;
    // Inspection may retain a worker with an arbitrary fallback harness.
    // That cached handle must not make the profile recoverable.
    let owner = store.session(&session)?.parent_session_id.unwrap();
    let inspected = manager.find_child(&owner, &session, false)?;
    assert!(!inspected.original_profile);
    let error = manager
        .resume_quota_wait(
            &store.quota_wait(&session)?.unwrap(),
            ApprovalPolicy::default(),
        )
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("original agent profile internal-worker-gone is unavailable")
    );
    assert_eq!(probe.0.load(Ordering::SeqCst), 0);
    assert_eq!(store.quota_wait(&session)?.unwrap().run_id, old_run);
    assert_eq!(store.runs(&session)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn busy_child_recovery_keeps_the_marker_for_a_later_attempt() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (store, session, old_run) = restored_wait(dir.path(), "read-only", false)?;
    let probe = Arc::new(Probe(AtomicUsize::new(0)));
    let manager = manager(&store, probe.clone())?;
    let owner = store.session(&session)?.parent_session_id.unwrap();
    let child = manager.child(&owner, &session)?;
    child.turn.lock().unwrap().result = None;
    let error = manager
        .resume_quota_wait(
            &store.quota_wait(&session)?.unwrap(),
            ApprovalPolicy::default(),
        )
        .err()
        .unwrap();
    assert!(
        error
            .downcast_ref::<bubblepaw_core::SessionBusy>()
            .is_some()
    );
    assert_eq!(store.quota_wait(&session)?.unwrap().run_id, old_run);
    assert_eq!(probe.0.load(Ordering::SeqCst), 0);
    child.turn.lock().unwrap().result = Some(json!({"status":"idle"}));
    Ok(())
}

#[tokio::test]
async fn quota_wait_status_stays_pending_without_consuming_a_child_result() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (store, session, old_run) = restored_wait(dir.path(), "read-only", false)?;
    let manager = manager(&store, Arc::new(Probe(AtomicUsize::new(0))))?;
    let owner = store.session(&session)?.parent_session_id.unwrap();
    let archived = manager.archived_status(&session)?;
    assert_eq!(archived["status"], "waiting_for_quota");
    let child = manager.child(&owner, &session)?;
    let reply = snapshot(&child, true)?;
    assert_eq!(reply["status"], "waiting_for_quota");
    assert_eq!(reply["quotaWait"]["run_id"], old_run);
    assert!(
        store
            .attachment(&session, &format!("{READ_PREFIX}{old_run}"))?
            .is_none()
    );
    let continuation = manager.continuation_status(&owner);
    assert_eq!(continuation["entries"][0]["status"], "waiting_for_quota");
    assert_eq!(continuation["entries"][0]["resultAvailable"], false);
    Ok(())
}

#[tokio::test]
async fn quota_wait_marker_prevents_interrupted_worker_retention_cleanup() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (store, session, _) = restored_wait(dir.path(), "internal-worker-gone", true)?;
    let manager = manager(&store, Arc::new(Probe(AtomicUsize::new(0))))?;
    let removed = cleanup_workers_at(
        &store,
        &manager.registry,
        &Agents {
            worker_retention_days: 0,
            ..Default::default()
        },
        chrono::Utc::now() + chrono::Duration::days(365),
    )?;
    assert_eq!(removed, 0);
    assert!(store.quota_wait(&session)?.is_some());
    Ok(())
}

#[tokio::test]
async fn stale_quota_schedule_does_not_start_a_child_without_its_marker() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (store, session, old_run) = restored_wait(dir.path(), "read-only", false)?;
    let expected = store.quota_wait(&session)?.unwrap();
    let probe = Arc::new(Probe(AtomicUsize::new(0)));
    let manager = manager(&store, probe.clone())?;
    store.clear_quota_wait(&session, &old_run)?;
    let error = manager
        .resume_quota_wait(&expected, ApprovalPolicy::default())
        .err()
        .unwrap();
    assert!(
        error
            .downcast_ref::<bubblepaw_core::QuotaWaitChanged>()
            .is_some()
    );
    assert_eq!(probe.0.load(Ordering::SeqCst), 0);
    assert_eq!(store.runs(&session)?.len(), 1);
    Ok(())
}
