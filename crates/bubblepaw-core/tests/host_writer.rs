//! The host preparation hook writes per-request context through ContextWriter.
//! Reminders land at the tail; hooks and later resolver values cannot replace
//! the chat's persisted system prompt.
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, Harness, HarnessConfig, ModelRequest, ModelResponse,
    Observation, ObservationMode, ObservationRecord, ObservationSink, PrepareCall, PrepareMode,
    PreparedContext, Provider, RunOptions, Store, TextCallback, ToolRegistry,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Capture(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for Capture {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "ok".into(),
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct Records(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for Records {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

/// A host hook with one live fact. `edit_prompt` writes it into the system
/// prompt the old way; otherwise it is a `project` reminder.
#[derive(Default)]
struct Host {
    project: Mutex<String>,
    edit_prompt: bool,
}
#[async_trait]
impl ContextPreparation for Host {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let mut request = call.request;
        let writer = call.writer;
        let project = self.project.lock().unwrap().clone();
        if self.edit_prompt {
            request
                .system_prompt
                .push_str(&format!("\nCurrent project: {project}"));
        } else {
            writer.remind("project", &format!("Current project: {project}"));
        }
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
}

/// Writes the manual compaction focus as a reminder, the way a host would.
#[derive(Default)]
struct FocusHost;
#[async_trait]
impl ContextPreparation for FocusHost {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        if let PrepareMode::Compact { focus } = call.mode {
            call.writer
                .remind("compaction_focus", focus.unwrap_or("none"));
        }
        Ok(PreparedContext {
            request: call.request,
            notice: None,
            usage: None,
        })
    }
}

struct Fixture {
    harness: Harness,
    provider: Arc<Capture>,
    records: Arc<Records>,
    session: String,
}
fn fixture(
    host: Arc<Host>,
    resolver: Option<bubblepaw_core::SystemPromptResolver>,
) -> Result<Fixture> {
    fixture_with(host, resolver)
}
fn fixture_with(
    context: Arc<dyn ContextPreparation>,
    resolver: Option<bubblepaw_core::SystemPromptResolver>,
) -> Result<Fixture> {
    let store = Store::in_memory()?;
    let session = store.create_session("host writer")?.id;
    let provider = Arc::new(Capture::default());
    let records = Arc::new(Records::default());
    let harness = Harness::new(
        store,
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context,
            system_prompt_resolver: resolver,
            observation: Some(Observation {
                sink: records.clone(),
                mode: ObservationMode::Metadata,
                max_payload_bytes: 0,
            }),
            ..Default::default()
        },
    );
    Ok(Fixture {
        harness,
        provider,
        records,
        session,
    })
}
impl Fixture {
    async fn prompt(&self, text: &str) -> Result<()> {
        let run = self
            .harness
            .prompt(&self.session, text, RunOptions::default())
            .await?;
        if run.status != "completed" {
            return Err(anyhow!("run ended {}", run.status));
        }
        Ok(())
    }
    /// context_built metrics of the root requests, in order.
    fn builds(&self) -> Vec<serde_json::Value> {
        self.records
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.kind == "context_built")
            .map(|record| record.metrics.clone())
            .collect()
    }
}

#[tokio::test]
async fn hook_reminders_land_at_the_tail_and_are_sent_only_when_changed() -> Result<()> {
    let host = Arc::new(Host::default());
    *host.project.lock().unwrap() = "alpha".into();
    let f = fixture(host.clone(), None)?;
    f.prompt("one").await?;
    f.prompt("two").await?;
    *host.project.lock().unwrap() = "beta".into();
    f.prompt("three").await?;
    let requests = f.provider.0.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let tail = |index: usize| requests[index].messages.last().unwrap().content.clone();
    assert_eq!(
        tail(0),
        "<harness kind=\"project\">Current project: alpha</harness>"
    );
    assert_eq!(tail(1), "two", "unchanged text is not re-sent");
    assert_eq!(
        tail(2),
        "<harness kind=\"project\">Current project: beta</harness>"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.system_prompt == requests[0].system_prompt),
        "the system prompt is byte-identical"
    );
    let kinds: Vec<_> = f
        .builds()
        .iter()
        .map(|build| build["classification"]["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["first", "append", "append"]);
    Ok(())
}

#[tokio::test]
async fn attempted_hook_prompt_edits_cannot_replace_the_chat_prefix() -> Result<()> {
    let host = Arc::new(Host {
        edit_prompt: true,
        ..Default::default()
    });
    *host.project.lock().unwrap() = "alpha".into();
    let f = fixture(host.clone(), None)?;
    f.prompt("one").await?;
    f.prompt("two").await?;
    *host.project.lock().unwrap() = "beta".into();
    f.prompt("three").await?;
    let builds = f.builds();
    assert_eq!(builds.len(), 3);
    assert_eq!(builds[1]["classification"]["kind"], "append");
    assert!(builds[1].get("frozen_writer").is_none());
    assert_eq!(builds[2]["classification"]["kind"], "append");
    assert_eq!(builds[2]["declared"], false);
    assert!(builds[2]["declaration_reason"].is_null());
    assert!(builds[2].get("frozen_writer").is_none());
    let requests = f.provider.0.lock().unwrap();
    for request in requests.iter() {
        assert_eq!(request.system_prompt, requests[0].system_prompt);
        assert_eq!(
            serde_json::to_vec(&request.tools)?,
            serde_json::to_vec(&requests[0].tools)?
        );
        assert!(!request.system_prompt.contains("Current project:"));
    }
    Ok(())
}

#[tokio::test]
async fn later_resolver_values_apply_only_to_new_chats() -> Result<()> {
    let version = Arc::new(Mutex::new(1));
    let resolver_version = version.clone();
    let resolver: bubblepaw_core::SystemPromptResolver = Arc::new(move |_, base| {
        Ok(format!(
            "{base}\nMemory v{}",
            resolver_version.lock().unwrap()
        ))
    });
    let f = fixture(Arc::new(Host::default()), Some(resolver))?;
    f.prompt("one").await?;
    f.prompt("two").await?;
    *version.lock().unwrap() = 2;
    f.prompt("three").await?;
    let builds = f.builds();
    assert_eq!(builds.len(), 3);
    assert_eq!(builds[1]["classification"]["kind"], "append");
    assert_eq!(builds[2]["classification"]["kind"], "append");
    assert_eq!(builds[2]["declared"], false);
    assert!(builds[2]["declaration_reason"].is_null());
    assert!(builds[2].get("frozen_writer").is_none());
    {
        let requests = f.provider.0.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request.system_prompt == requests[0].system_prompt)
        );
        assert!(requests[2].system_prompt.ends_with("Memory v1"));
    }
    let fresh = f
        .harness
        .store()
        .create_session("fresh resolver context")?
        .id;
    let run = f
        .harness
        .prompt(&fresh, "fresh", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed");
    assert!(
        f.provider
            .0
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .system_prompt
            .ends_with("Memory v2")
    );
    Ok(())
}

#[tokio::test]
async fn compaction_focus_written_by_the_hook_reaches_the_next_request_tail() -> Result<()> {
    let f = fixture_with(Arc::new(FocusHost), None)?;
    f.harness
        .compact_session(&f.session, Some("Keep the schema decisions"))
        .await?;
    f.prompt("after compaction").await?;
    let requests = f.provider.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].messages.last().unwrap().content,
        "<harness kind=\"compaction_focus\">Keep the schema decisions</harness>"
    );
    Ok(())
}

#[tokio::test]
async fn budget_recovery_without_frontier_progress_stops_without_delivering_reminders() -> Result<()>
{
    #[derive(Default)]
    struct StalledHost(Mutex<Vec<PrepareMode<'static>>>);
    #[async_trait]
    impl ContextPreparation for StalledHost {
        async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
            let mode = match call.mode {
                PrepareMode::Request => PrepareMode::Request,
                PrepareMode::Pressure {
                    additional_tokens,
                    additional_bytes,
                } => PrepareMode::Pressure {
                    additional_tokens,
                    additional_bytes,
                },
                PrepareMode::Compact { .. } => unreachable!(),
            };
            self.0.lock().unwrap().push(mode);
            call.writer.remind("fixture", "pending until admitted");
            Ok(PreparedContext {
                request: call.request,
                notice: None,
                usage: None,
            })
        }
        fn validate_prepared_request(
            &self,
            _: &str,
            _: &ModelRequest,
            _: Arc<dyn Provider>,
        ) -> Result<()> {
            Err(bubblepaw_core::ContextBudgetExceeded {
                additional_tokens: 7,
                additional_bytes: 23,
                message: "Prepared request including harness reminders exceeds the context budget (101 > 100 tokens)".into(),
            }.into())
        }
    }
    let host = Arc::new(StalledHost::default());
    let f = fixture_with(host.clone(), None)?;
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.harness
            .prompt(&f.session, "stay bounded", RunOptions::default()),
    )
    .await??;
    assert_eq!(run.status, "failed");
    assert_eq!(
        run.error.as_deref(),
        Some(
            "Prepared request including harness reminders exceeds the context budget (101 > 100 tokens)"
        )
    );
    assert_eq!(
        *host.0.lock().unwrap(),
        vec![
            PrepareMode::Request,
            // The newly staged reminder first re-enters ordinary preparation.
            PrepareMode::Request,
            PrepareMode::Pressure {
                additional_tokens: 7,
                additional_bytes: 23
            }
        ]
    );
    assert!(f.provider.0.lock().unwrap().is_empty());
    assert!(
        f.builds().is_empty(),
        "rejected candidates never advance the request log"
    );
    let history = f.harness.store().messages(&f.session)?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].content, "stay bounded");
    Ok(())
}
