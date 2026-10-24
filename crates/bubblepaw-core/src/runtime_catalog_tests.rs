use super::*;
use crate::{ObservationMode, ObservationRecord, ObservationSink};

#[derive(Default)]
struct Records(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for Records {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

struct Answer;
#[async_trait]
impl Provider for Answer {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}

struct ColdHook {
    view: Message,
    note: Mutex<String>,
}
#[async_trait]
impl ContextPreparation for ColdHook {
    fn project_history(&self, history: &[Message]) -> Result<Vec<Message>> {
        Ok(std::iter::once(self.view.clone())
            .chain(history.iter().cloned())
            .collect())
    }
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        call.writer
            .cold_note("host_note", &self.note.lock().unwrap());
        Ok(PreparedContext {
            request: call.request,
            notice: None,
            usage: None,
        })
    }
    fn cold_message_indexes(&self, _: &str, _: &ModelRequest) -> Result<Vec<usize>> {
        Ok(vec![0])
    }
}

#[tokio::test]
async fn registry_generation_observation_does_not_refresh_frozen_or_cold_tiers() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("registry observation")?.id;
    let registry = Arc::new(Mutex::new(ToolRegistry::new().with_generation(1)));
    let source = registry.clone();
    let records = Arc::new(Records::default());
    let context = Arc::new(ColdHook {
        view: Store::message(&session, Role::System, "fixed view".into()),
        note: Mutex::new("initial note".into()),
    });
    let harness = Harness::new(
        store,
        Arc::new(Answer),
        ToolRegistry::new(),
        HarnessConfig {
            registry_resolver: Some(Arc::new(move || source.lock().unwrap().clone())),
            context: context.clone(),
            observation: Some(Observation {
                sink: records.clone(),
                mode: ObservationMode::Metadata,
                max_payload_bytes: 1024,
            }),
            ..Default::default()
        },
    );
    harness
        .prompt(&session, "first", RunOptions::default())
        .await?;
    *registry.lock().unwrap() = ToolRegistry::new().with_generation(2);
    *context.note.lock().unwrap() = "staged until apply".into();
    let run = harness
        .prompt(&session, "second", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let records = records.0.lock().unwrap();
    let changes: Vec<_> = records
        .iter()
        .filter(|record| record.kind == "context_change")
        .collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].scope.run_id, run.id);
    assert_eq!(
        changes[0].metrics,
        json!({"reason":"tools_reloaded","prefix_changed":false,"generation":2})
    );
    let builds: Vec<_> = records
        .iter()
        .filter(|record| record.kind == "context_built")
        .collect();
    assert_eq!(builds.len(), 2);
    assert_eq!(builds[0].metrics["frozen"], builds[1].metrics["frozen"]);
    assert_eq!(builds[0].metrics["cold"], builds[1].metrics["cold"]);
    assert_eq!(builds[1].metrics["classification"]["kind"], "append");
    assert_eq!(builds[1].metrics["declared"], false);
    assert!(!records.iter().any(|record| record.kind == "cache_bust"));
    Ok(())
}

#[path = "runtime_catalog_tests/pending.rs"]
mod pending;
#[path = "runtime_catalog_tests/updated.rs"]
mod updated;
