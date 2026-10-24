use bubblepaw_core::{Harness, HarnessConfig, Provider, Run, RunOptions, Store, ToolRegistry};
use std::sync::Arc;

/// One store, one harness and one session: a turn runs through the host's
/// ordinary `Harness::prompt` path, so provider, tool and run records are real.
#[allow(dead_code)]
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub store: Store,
    pub harness: Harness,
    pub session: String,
    model: String,
}
#[allow(dead_code)]
impl Fixture {
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry, model: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path().join("archive.sqlite")).unwrap();
        let harness = Harness::new(
            store.clone(),
            provider,
            tools,
            HarnessConfig {
                system_prompt: "Worker policy".into(),
                ..Default::default()
            },
        );
        let session = store.create_session("fixture").unwrap().id;
        Self {
            root,
            store,
            harness,
            session,
            model: model.into(),
        }
    }

    /// Runs one turn to idle and returns its durable run record.
    pub async fn run(&self, prompt: &str) -> Run {
        let options = RunOptions {
            model: Some(self.model.clone()),
            ..Default::default()
        };
        self.harness
            .prompt(&self.session, prompt, options)
            .await
            .unwrap()
    }
}
