//! Internal workers reuse ordinary child runs. All provider work is scripted.
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role, Store,
    TextCallback, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    agents::{AgentProfile, AgentTools},
    config::Agents,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

enum Step {
    Success,
    Failure,
    Hang,
}
struct Probe {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    entered: mpsc::UnboundedSender<()>,
    closed: Mutex<Vec<String>>,
    close_fails: bool,
    close_hangs: Mutex<bool>,
}
#[async_trait]
impl Provider for Probe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        match step {
            Step::Success => Ok(ModelResponse {
                text: "worker answer".into(),
                ..Default::default()
            }),
            Step::Failure => bail!("scripted worker failure"),
            Step::Hang => {
                self.entered.send(())?;
                std::future::pending().await
            }
        }
    }
    async fn close_session(&self, id: &str) -> Result<()> {
        self.closed.lock().unwrap().push(id.into());
        let hangs = *self.close_hangs.lock().unwrap();
        if hangs {
            std::future::pending::<()>().await;
        }
        if self.close_fails {
            bail!("scripted mirror cleanup failure");
        }
        Ok(())
    }
}
struct Fixture {
    directory: tempfile::TempDir,
    store: Store,
    manager: AgentTools,
    profile: AgentProfile,
    owner: ToolContext,
    probe: Arc<Probe>,
    entered: mpsc::UnboundedReceiver<()>,
}
impl Fixture {
    fn new(steps: Vec<Step>, limits: Agents) -> Result<Self> {
        Self::with_close(steps, limits, false)
    }
    fn with_close(steps: Vec<Step>, limits: Agents, close_fails: bool) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let parent = store.create_session("parent")?;
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let probe = Arc::new(Probe {
            steps: Mutex::new(steps.into()),
            requests: Mutex::default(),
            entered: entered_tx,
            closed: Mutex::default(),
            close_fails,
            close_hangs: Mutex::new(false),
        });
        let profile = AgentProfile {
            name: "internal-test".into(),
            description: "Scripted worker".into(),
            harness: Arc::new(Harness::new(
                store.clone(),
                probe.clone(),
                ToolRegistry::new(),
                HarnessConfig::default(),
            )),
            model: None,
            max_rounds: Some(4),
            approval: None,
        };
        let manager =
            AgentTools::from_profiles(store.clone(), vec![profile.clone()])?.with_limits(limits);
        Ok(Self {
            directory,
            store,
            manager,
            profile,
            owner: ToolContext {
                session_id: parent.id,
                run_id: "parent-run".into(),
                invocation_id: "worker-launch".into(),
                cancellation: CancellationToken::new(),
                approval: Default::default(),
            },
            probe,
            entered,
        })
    }
    fn start(&self, extra: Value) -> Result<Value> {
        let mut input = json!({"task":"perform a short internal task"});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.manager
            .start_worker(&self.owner, self.profile.clone(), input)
    }
    async fn wait(&self, worker: &Value) -> Result<Value> {
        let result = self
            .manager
            .wait(
                &self.owner.session_id,
                worker["sessionId"].as_str().unwrap(),
                Some(2000),
            )
            .await?;
        assert_ne!(
            result["status"], "timeout",
            "scripted worker did not finish"
        );
        Ok(result)
    }
    async fn entered(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), self.entered.recv())
            .await?
            .expect("provider entry");
        Ok(())
    }
    fn age(&self, worker: &Value) -> Result<()> {
        let conn =
            rusqlite::Connection::open(self.store.chat_path(&self.owner.session_id)?.unwrap())?;
        let run_id = worker["runId"].as_str().context("worker receipt run id")?;
        let updated = conn.execute("UPDATE attachments SET metadata=json_set(metadata,'$.finished_at','2000-01-01T00:00:00Z') WHERE session_id=?1 AND id=?2 AND json_extract(metadata,'$.run_id')=?3", rusqlite::params![worker["sessionId"].as_str().unwrap(), finished_id(run_id), run_id])?;
        assert_eq!(
            updated, 1,
            "age exactly the worker receipt's completion marker"
        );
        Ok(())
    }
}
fn finished_id(run_id: &str) -> String {
    format!("bubblepaw-worker-finished-v1:{run_id}")
}
#[tokio::test]
async fn successful_worker_is_hidden_readable_then_deleted_only_by_explicit_ack() -> Result<()> {
    let fixture = Fixture::new(vec![Step::Success], Agents::default())?;
    let worker = fixture.start(json!({}))?;
    let id = worker["sessionId"].as_str().unwrap();
    assert!(fixture.store.session(id)?.hidden);
    assert_eq!(fixture.store.list_sessions()?.len(), 2);
    assert_eq!(fixture.store.list_visible_sessions()?.len(), 1);
    assert_eq!(
        fixture.store.visible_session_summaries(1)?[0].session.id,
        fixture.owner.session_id
    );
    assert_eq!(
        fixture
            .manager
            .status(&fixture.owner.session_id, &json!({}))?["children"],
        json!([])
    );
    assert_eq!(
        fixture
            .manager
            .continuation_status(&fixture.owner.session_id)["entries"],
        json!([])
    );
    assert!(
        fixture
            .manager
            .take_reminders(&fixture.owner.session_id)
            .is_empty()
    );
    assert!(
        fixture
            .manager
            .acknowledge_worker(&fixture.owner.session_id, id)
            .await
            .is_err()
    );
    assert_eq!(fixture.wait(&worker).await?["text"], "worker answer");
    assert!(!fixture.store.messages(id)?.is_empty());
    assert_eq!(
        fixture.probe.closed.lock().unwrap().as_slice(),
        &[id.to_owned()]
    );
    assert!(
        fixture
            .manager
            .send(&fixture.owner, json!({"sessionId":id,"text":"continue"}))
            .is_err()
    );
    assert_eq!(
        fixture.store.runs(id)?.len(),
        1,
        "worker send rejects before creating a new turn"
    );
    fixture
        .manager
        .acknowledge_worker("another-owner", id)
        .await
        .unwrap_err();
    fixture
        .manager
        .acknowledge_worker(&fixture.owner.session_id, id)
        .await?;
    assert!(fixture.store.session(id).is_err());
    assert_eq!(fixture.store.list_sessions()?.len(), 1);
    assert_eq!(
        fixture.probe.closed.lock().unwrap().len(),
        1,
        "ack must not close a provider twice"
    );
    fixture
        .manager
        .acknowledge_worker(&fixture.owner.session_id, id)
        .await?;
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn mismatched_worker_run_receipt_cannot_delete_a_read_success() -> Result<()> {
    let fixture = Fixture::new(vec![Step::Success], Agents::default())?;
    let worker = fixture.start(json!({}))?;
    let id = worker["sessionId"].as_str().unwrap();
    let run_id = worker["runId"]
        .as_str()
        .expect("worker start receipt binds its run");
    assert_eq!(fixture.store.runs(id)?.last().unwrap().id, run_id);
    assert_eq!(fixture.wait(&worker).await?["status"], "completed");
    let error = fixture
        .manager
        .acknowledge_worker_turn(&fixture.owner.session_id, id, "stale-run-id")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("run does not match acknowledgement receipt")
    );
    assert!(fixture.store.session(id).is_ok());
    fixture
        .manager
        .acknowledge_worker_turn(&fixture.owner.session_id, id, run_id)
        .await?;
    assert!(fixture.store.session(id).is_err());
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn zero_or_expired_retention_cannot_delete_unacknowledged_success() -> Result<()> {
    for days in [0, 7] {
        let fixture = Fixture::new(
            vec![Step::Success],
            Agents {
                worker_retention_days: days,
                ..Default::default()
            },
        )?;
        let worker = fixture.start(json!({}))?;
        assert_eq!(fixture.wait(&worker).await?["status"], "completed");
        fixture.age(&worker)?;
        assert_eq!(fixture.manager.cleanup_workers()?, 0);
        let id = worker["sessionId"].as_str().unwrap();
        assert!(fixture.store.session(id).is_ok());
        fixture
            .manager
            .acknowledge_worker(&fixture.owner.session_id, id)
            .await?;
        assert!(fixture.store.session(id).is_err());
        fixture.manager.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn failure_cancellation_and_timeout_are_retained_and_expire_after_policy() -> Result<()> {
    let mut fixture = Fixture::new(
        vec![Step::Failure, Step::Hang, Step::Hang],
        Agents::default(),
    )?;
    let failed = fixture.start(json!({}))?;
    assert_eq!(fixture.wait(&failed).await?["status"], "failed");
    let cancelled = fixture.start(json!({}))?;
    fixture.entered().await?;
    fixture.owner.cancellation.cancel();
    assert_eq!(fixture.wait(&cancelled).await?["status"], "cancelled");
    fixture.owner.cancellation = CancellationToken::new();
    let timed_out = fixture.start(json!({"timeout":40}))?;
    fixture.entered().await?;
    assert_eq!(fixture.wait(&timed_out).await?["status"], "timed_out");
    for worker in [&failed, &cancelled, &timed_out] {
        let id = worker["sessionId"].as_str().unwrap();
        fixture
            .manager
            .acknowledge_worker(&fixture.owner.session_id, id)
            .await?;
        assert!(fixture.store.session(id).is_ok());
    }
    assert_eq!(fixture.manager.cleanup_workers()?, 0);
    assert_eq!(
        fixture.probe.closed.lock().unwrap().len(),
        3,
        "retention must not keep live provider mirrors"
    );
    for worker in [&failed, &cancelled, &timed_out] {
        fixture.age(worker)?;
    }
    assert_eq!(fixture.manager.cleanup_workers()?, 3);
    assert_eq!(fixture.store.list_sessions()?.len(), 1);
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancelling_a_completed_worker_keeps_the_chat_for_failure_retention() -> Result<()> {
    let fixture = Fixture::new(vec![Step::Success], Agents::default())?;
    let worker = fixture.start(json!({}))?;
    assert_eq!(fixture.wait(&worker).await?["status"], "completed");
    let id = worker["sessionId"].as_str().unwrap();
    assert_eq!(
        fixture
            .manager
            .cancel(&fixture.owner.session_id, id)
            .await?["status"],
        "cancelled"
    );
    fixture
        .manager
        .acknowledge_worker(&fixture.owner.session_id, id)
        .await?;
    assert!(fixture.store.session(id).is_ok());
    assert_eq!(fixture.manager.cleanup_workers()?, 0);
    fixture.age(&worker)?;
    assert_eq!(fixture.manager.cleanup_workers()?, 1);
    assert!(fixture.store.session(id).is_err());
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn parent_ack_cannot_erase_a_separately_cancelled_completed_descendant() -> Result<()> {
    let fixture = Fixture::new(vec![Step::Success, Step::Success], Agents::default())?;
    let outer = fixture.start(json!({})).context("start outer worker")?;
    assert_eq!(
        fixture
            .wait(&outer)
            .await
            .context("read outer worker result")?["status"],
        "completed"
    );
    let outer_id = outer["sessionId"].as_str().unwrap();
    let mut owner = fixture.owner.clone();
    owner.session_id = outer_id.into();
    owner.invocation_id = "nested-worker-launch".into();
    let inner = fixture
        .manager
        .start_worker(
            &owner,
            fixture.profile.clone(),
            json!({"task":"nested internal task"}),
        )
        .context("start nested worker")?;
    let inner_id = inner["sessionId"].as_str().unwrap();
    assert_eq!(
        fixture
            .manager
            .wait(outer_id, inner_id, None)
            .await
            .context("read nested worker result")?["status"],
        "completed"
    );
    assert_eq!(
        fixture
            .manager
            .cancel(outer_id, inner_id)
            .await
            .context("cancel completed nested worker")?["status"],
        "cancelled"
    );
    let error = fixture
        .manager
        .acknowledge_worker(&fixture.owner.session_id, outer_id)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled children"));
    assert!(fixture.store.session(outer_id).is_ok());
    assert!(fixture.store.session(inner_id).is_ok());
    fixture
        .age(&inner)
        .context("age nested cancellation retention marker")?;
    assert_eq!(
        fixture
            .manager
            .cleanup_workers()
            .context("expire nested worker while retaining successful ancestor")?,
        1
    );
    assert!(fixture.store.session(inner_id).is_err());
    fixture
        .manager
        .acknowledge_worker(&fixture.owner.session_id, outer_id)
        .await
        .context("ack outer after expired descendant deletion")?;
    assert!(fixture.store.session(outer_id).is_err());
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn parent_ack_waits_for_nested_worker_cleanup_and_preserves_archived_child_failure()
-> Result<()> {
    let fixture = Fixture::new(
        vec![Step::Success, Step::Success, Step::Success],
        Agents::default(),
    )?;
    let outer = fixture.start(json!({}))?;
    assert_eq!(fixture.wait(&outer).await?["status"], "completed");
    let outer_id = outer["sessionId"].as_str().unwrap();
    *fixture.probe.close_hangs.lock().unwrap() = true;
    let mut owner = fixture.owner.clone();
    owner.session_id = outer_id.into();
    owner.invocation_id = "nested-cleanup-worker".into();
    let inner = fixture.manager.start_worker(
        &owner,
        fixture.profile.clone(),
        json!({"task":"nested task awaiting mirror cleanup"}),
    )?;
    let inner_id = inner["sessionId"].as_str().unwrap();
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if fixture
                .probe
                .closed
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == inner_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .context("nested provider entered pending mirror cleanup")?;
    assert_eq!(
        fixture.store.runs(inner_id)?.last().unwrap().status,
        "completed"
    );
    assert!(
        fixture
            .store
            .attachment(inner_id, &finished_id(inner["runId"].as_str().unwrap()))?
            .is_none()
    );
    let error = fixture
        .manager
        .acknowledge_worker(&fixture.owner.session_id, outer_id)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cleanup is still in progress"));
    assert!(fixture.store.session(outer_id).is_ok());
    assert!(fixture.store.session(inner_id).is_ok());

    // No process-local handles: durable binding must still require the nested
    // worker's cleanup publication rather than accepting its core completion.
    let inspector =
        AgentTools::from_profiles(fixture.store.clone(), vec![fixture.profile.clone()])?;
    let error = inspector
        .acknowledge_worker(&fixture.owner.session_id, outer_id)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("workers awaiting cleanup publication")
    );
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        fixture.manager.wait(outer_id, inner_id, None),
    )
    .await??;
    assert_eq!(result["status"], "failed");
    assert!(
        fixture
            .manager
            .acknowledge_worker(&fixture.owner.session_id, outer_id)
            .await
            .is_err()
    );
    assert!(fixture.store.session(inner_id).is_ok());
    fixture.age(&inner)?;
    assert_eq!(fixture.manager.cleanup_workers()?, 1);

    // Freeze an ordinary hidden child's host cleanup failure after its core run
    // completed. A fresh inspector must honor the existing failure attachment.
    *fixture.probe.close_hangs.lock().unwrap() = false;
    let ordinary = fixture.manager.start(
        &owner,
        json!({"task":"ordinary hidden child", "profile":"internal-test"}),
    )?;
    let ordinary_id = ordinary["sessionId"].as_str().unwrap();
    assert_eq!(
        fixture.manager.wait(outer_id, ordinary_id, None).await?["status"],
        "completed"
    );
    assert!(fixture.store.session(ordinary_id)?.hidden);
    let run = fixture.store.runs(ordinary_id)?.into_iter().last().unwrap();
    fixture.store.store_attachment(
        ordinary_id,
        &format!("bubblepaw-agent-failure-v1:{}", run.id),
        &json!({"status":"failed","text":"worker answer","error":"scripted child cleanup failure"})
            .to_string(),
        b"",
    )?;
    let error = inspector
        .acknowledge_worker(&fixture.owner.session_id, outer_id)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("child cleanup or control failures")
    );
    assert!(fixture.store.session(outer_id).is_ok());
    assert!(fixture.store.session(ordinary_id).is_ok());
    inspector.shutdown().await?;
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn expired_failed_worker_can_reclaim_a_successful_ordinary_hidden_descendant() -> Result<()> {
    let fixture = Fixture::new(
        vec![Step::Failure, Step::Success],
        Agents {
            worker_retention_days: 0,
            ..Default::default()
        },
    )?;
    let outer = fixture.start(json!({}))?;
    assert_eq!(fixture.wait(&outer).await?["status"], "failed");
    let outer_id = outer["sessionId"].as_str().unwrap();
    let mut owner = fixture.owner.clone();
    owner.session_id = outer_id.into();
    owner.invocation_id = "ordinary-descendant-after-failure".into();
    let child = fixture.manager.start(
        &owner,
        json!({"task":"ordinary resolved child", "profile":"internal-test"}),
    )?;
    let child_id = child["sessionId"].as_str().unwrap();
    assert_eq!(
        fixture.manager.wait(outer_id, child_id, None).await?["status"],
        "completed"
    );
    assert!(fixture.store.session(child_id)?.hidden);
    assert!(
        fixture
            .store
            .attachment(child_id, "bubblepaw-worker-finished-v1")?
            .is_none()
    );
    let child_run = fixture.store.runs(child_id)?.into_iter().last().unwrap();
    assert!(
        fixture
            .store
            .attachment(child_id, &finished_id(&child_run.id))?
            .is_none()
    );
    assert_eq!(
        fixture
            .probe
            .closed
            .lock()
            .unwrap()
            .iter()
            .filter(|id| id.as_str() == child_id)
            .count(),
        1,
        "ordinary hidden mirror must close before retention deletion"
    );
    assert_eq!(fixture.manager.cleanup_workers()?, 2);
    assert!(fixture.store.session(outer_id).is_err());
    assert!(fixture.store.session(child_id).is_err());
    assert_eq!(
        fixture
            .probe
            .closed
            .lock()
            .unwrap()
            .iter()
            .filter(|id| id.as_str() == child_id)
            .count(),
        1,
        "retention must not close a mirror twice"
    );
    assert_eq!(fixture.store.list_sessions()?.len(), 1);
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_hidden_child_continues_from_canonical_history_after_mirror_release() -> Result<()>
{
    let fixture = Fixture::new(
        vec![Step::Success, Step::Success, Step::Success],
        Agents::default(),
    )?;
    let outer = fixture.start(json!({}))?;
    assert_eq!(fixture.wait(&outer).await?["status"], "completed");
    let outer_id = outer["sessionId"].as_str().unwrap();
    let mut owner = fixture.owner.clone();
    owner.session_id = outer_id.into();
    owner.invocation_id = "ordinary-hidden-continuation".into();
    let child = fixture.manager.start(
        &owner,
        json!({"task":"ordinary first task", "profile":"internal-test"}),
    )?;
    let child_id = child["sessionId"].as_str().unwrap();
    assert_eq!(
        fixture.manager.wait(outer_id, child_id, None).await?["status"],
        "completed"
    );
    assert!(fixture.store.session(child_id)?.hidden);
    assert_eq!(fixture.store.messages(child_id)?.len(), 2);
    fixture.manager.send(
        &owner,
        json!({"sessionId":child_id, "text":"ordinary follow-up task"}),
    )?;
    assert_eq!(
        fixture.manager.wait(outer_id, child_id, None).await?["status"],
        "completed"
    );
    assert_eq!(fixture.store.runs(child_id)?.len(), 2);
    assert_eq!(fixture.store.messages(child_id)?.len(), 4);
    {
        let requests = fixture.probe.requests.lock().unwrap();
        let child_requests: Vec<_> = requests
            .iter()
            .filter(|request| request.session_id == child_id)
            .collect();
        assert_eq!(child_requests.len(), 2);
        assert!(
            child_requests[1]
                .messages
                .iter()
                .any(|message| message.role == Role::User
                    && message.content == "ordinary first task")
        );
        assert!(
            child_requests[1].messages.iter().any(
                |message| message.role == Role::Assistant && message.content == "worker answer"
            )
        );
        assert!(
            child_requests[1]
                .messages
                .iter()
                .any(|message| message.role == Role::User
                    && message.content == "ordinary follow-up task")
        );
        assert_ne!(
            child_requests[0].history.epoch,
            child_requests[1].history.epoch
        );
    }
    assert_eq!(
        fixture
            .probe
            .closed
            .lock()
            .unwrap()
            .iter()
            .filter(|id| id.as_str() == child_id)
            .count(),
        2
    );
    fixture
        .manager
        .acknowledge_worker(&fixture.owner.session_id, outer_id)
        .await?;
    assert!(fixture.store.session(child_id).is_err());
    assert_eq!(
        fixture
            .probe
            .closed
            .lock()
            .unwrap()
            .iter()
            .filter(|id| id.as_str() == child_id)
            .count(),
        2,
        "parent acknowledgement must not repeat hidden-child cleanup"
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn keep_all_preserves_acknowledged_success_and_expired_failure() -> Result<()> {
    let fixture = Fixture::new(
        vec![Step::Success, Step::Failure],
        Agents {
            worker_retention_days: 0,
            worker_keep_all: true,
            ..Default::default()
        },
    )?;
    for expected in ["completed", "failed"] {
        let worker = fixture.start(json!({}))?;
        assert_eq!(fixture.wait(&worker).await?["status"], expected);
        fixture
            .manager
            .acknowledge_worker(
                &fixture.owner.session_id,
                worker["sessionId"].as_str().unwrap(),
            )
            .await?;
        fixture.age(&worker)?;
    }
    assert_eq!(fixture.manager.cleanup_workers()?, 0);
    assert_eq!(fixture.store.list_sessions()?.len(), 3);
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn provider_mirror_cleanup_failure_is_a_retained_worker_failure() -> Result<()> {
    let fixture = Fixture::with_close(vec![Step::Success], Agents::default(), true)?;
    let worker = fixture.start(json!({}))?;
    let result = fixture.wait(&worker).await?;
    assert_eq!(result["status"], "failed");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("mirror cleanup failure")
    );
    fixture
        .manager
        .acknowledge_worker(
            &fixture.owner.session_id,
            worker["sessionId"].as_str().unwrap(),
        )
        .await?;
    assert!(
        fixture
            .store
            .session(worker["sessionId"].as_str().unwrap())
            .is_ok()
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn hanging_provider_cleanup_has_a_bound_and_retains_a_failed_worker() -> Result<()> {
    let fixture = Fixture::new(vec![Step::Success], Agents::default())?;
    *fixture.probe.close_hangs.lock().unwrap() = true;
    let worker = fixture.start(json!({}))?;
    let id = worker["sessionId"].as_str().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        fixture.manager.wait(&fixture.owner.session_id, id, None),
    )
    .await??;
    assert_eq!(result["status"], "failed");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("provider cleanup timed out after 1 second")
    );
    fixture
        .manager
        .acknowledge_worker(&fixture.owner.session_id, id)
        .await?;
    assert!(fixture.store.session(id).is_ok());
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rejected_worker_binding_write_does_not_leave_an_invisible_orphan() -> Result<()> {
    let fixture = Fixture::new(vec![], Agents::default())?;
    let conn =
        rusqlite::Connection::open(fixture.store.chat_path(&fixture.owner.session_id)?.unwrap())?;
    conn.execute_batch("CREATE TRIGGER reject_worker_binding BEFORE INSERT ON attachments WHEN NEW.id='bubblepaw-agent-binding-v1' BEGIN SELECT RAISE(ABORT,'scripted binding write rejection'); END;")?;
    let error = fixture.start(json!({})).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("scripted binding write rejection")
    );
    assert_eq!(fixture.store.list_sessions()?.len(), 1);
    assert!(fixture.probe.closed.lock().unwrap().is_empty());
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn supplied_profile_cannot_use_an_unrelated_store_or_escape_configured_caps() -> Result<()> {
    let mut fixture = Fixture::new(
        vec![Step::Hang],
        Agents {
            max_active_total: Some(1),
            ..Default::default()
        },
    )?;
    let unrelated = Store::in_memory()?;
    let mut profile = fixture.profile.clone();
    profile.harness = Arc::new(Harness::new(
        unrelated,
        fixture.probe.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    assert!(
        fixture
            .manager
            .start_worker(&fixture.owner, profile, json!({"task":"wrong store"}))
            .is_err()
    );
    assert_eq!(fixture.store.list_sessions()?.len(), 1);
    let worker = fixture.start(json!({}))?;
    fixture.entered().await?;
    assert!(fixture.start(json!({})).is_err());
    fixture.owner.cancellation.cancel();
    assert_eq!(fixture.wait(&worker).await?["status"], "cancelled");
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn success_survives_restart_and_can_be_read_then_acknowledged_by_session_id() -> Result<()> {
    let fixture = Fixture::new(vec![Step::Success], Agents::default())?;
    let worker = fixture.start(json!({}))?;
    assert_eq!(fixture.wait(&worker).await?["status"], "completed");
    let id = worker["sessionId"].as_str().unwrap().to_owned();
    fixture.manager.shutdown().await?;
    let Fixture {
        directory,
        store: old_store,
        manager: old_manager,
        profile: old_profile,
        owner,
        probe: old_probe,
        entered: old_entered,
    } = fixture;
    drop((old_store, old_manager, old_profile, old_probe, old_entered));
    // Use a fresh physical store and harness; the dynamic profile name changed.
    let store = Store::open_data_dir(directory.path())?;
    assert!(store.session(&id)?.hidden);
    let provider = Arc::new(bubblepaw_runner::provider::ScriptedProvider::new("unused"));
    let manager = AgentTools::new(
        store.clone(),
        Harness::new(
            store.clone(),
            provider,
            ToolRegistry::new(),
            HarnessConfig::default(),
        ),
        None,
    );
    assert_eq!(
        manager.wait(&owner.session_id, &id, None).await?["text"],
        "worker answer"
    );
    manager.acknowledge_worker(&owner.session_id, &id).await?;
    assert!(store.session(&id).is_err());
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn hidden_descendants_and_unknown_effects_block_automatic_and_ack_deletion() -> Result<()> {
    let fixture = Fixture::new(vec![Step::Success], Agents::default())?;
    let worker = fixture.start(json!({}))?;
    assert_eq!(fixture.wait(&worker).await?["status"], "completed");
    let id = worker["sessionId"].as_str().unwrap();
    let child = fixture.store.create_session_with_parent(
        "internal descendant",
        Default::default(),
        Some(id),
    )?;
    assert!(child.hidden);
    let conn = rusqlite::Connection::open(fixture.store.chat_path(id)?.unwrap())?;
    conn.execute("INSERT INTO effects(id,session_id,run_id,call_id,state,detail) VALUES('uncertain',?1,'frozen-run','frozen-call','unknown','{}')", [&child.id])?;
    assert!(
        fixture
            .manager
            .acknowledge_worker(&fixture.owner.session_id, id)
            .await
            .is_err()
    );
    assert!(fixture.store.delete_session_preserving_effects(id).is_err());
    assert_eq!(fixture.manager.cleanup_workers()?, 0);
    assert!(fixture.store.session(&child.id).is_ok());
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn one_hundred_short_workers_reclaim_transcripts_and_bound_database_and_wal_growth()
-> Result<()> {
    let fixture = Fixture::new((0..200).map(|_| Step::Success).collect(), Agents::default())?;
    fixture.store.idle_checkpoint()?;
    let before = fixture.store.storage_stats()?;
    let mut samples = Vec::new();
    for batch in 0..2 {
        let batch_before = fixture.store.storage_stats()?;
        let started = std::time::Instant::now();
        let mut sampled_peak_database_bytes = batch_before.database_bytes;
        let mut sampled_peak_wal_bytes = batch_before.wal_bytes;
        for _ in 0..100 {
            let worker = fixture.start(json!({}))?;
            assert_eq!(fixture.wait(&worker).await?["status"], "completed");
            let retained = fixture.store.storage_stats()?;
            sampled_peak_database_bytes = sampled_peak_database_bytes.max(retained.database_bytes);
            sampled_peak_wal_bytes = sampled_peak_wal_bytes.max(retained.wal_bytes);
            fixture
                .manager
                .acknowledge_worker(
                    &fixture.owner.session_id,
                    worker["sessionId"].as_str().unwrap(),
                )
                .await?;
            let deleted = fixture.store.storage_stats()?;
            sampled_peak_database_bytes = sampled_peak_database_bytes.max(deleted.database_bytes);
            sampled_peak_wal_bytes = sampled_peak_wal_bytes.max(deleted.wal_bytes);
        }
        assert_eq!(fixture.store.list_sessions()?.len(), 1);
        let before_checkpoint = fixture.store.storage_stats()?;
        assert_eq!(before_checkpoint.message_count, 0);
        assert!(
            sampled_peak_wal_bytes <= 8 * 1024 * 1024,
            "WAL growth must remain bounded between checkpoints"
        );
        assert!(sampled_peak_database_bytes <= before.database_bytes + 1024 * 1024);
        let elapsed_ms = started.elapsed().as_millis();
        fixture.store.idle_checkpoint()?;
        let after = fixture.store.storage_stats()?;
        assert_eq!(after.wal_bytes, 0);
        assert!(after.database_bytes <= before.database_bytes + 512 * 1024);
        assert!(after.transaction_count > before.transaction_count);
        samples.push(json!({
            "batch":batch + 1, "workers":100, "elapsed_with_metadata_sampling_ms":elapsed_ms,
            "sampled_peak_database_bytes":sampled_peak_database_bytes,
            "sampled_peak_wal_bytes":sampled_peak_wal_bytes,
            "store_transaction_delta":before_checkpoint.transaction_count - batch_before.transaction_count,
            "all_file_transaction_delta":(before_checkpoint.chat_transaction_count + before_checkpoint.serving_transaction_count + before_checkpoint.catalog_transaction_count)
                - (batch_before.chat_transaction_count + batch_before.serving_transaction_count + batch_before.catalog_transaction_count),
            "before_checkpoint":before_checkpoint, "after_checkpoint":after
        }));
    }
    assert_eq!(fixture.probe.closed.lock().unwrap().len(), 200);
    println!(
        "worker storage measurement: {}",
        json!({"before":before,"after_batches":samples})
    );
    fixture.manager.shutdown().await?;
    Ok(())
}
