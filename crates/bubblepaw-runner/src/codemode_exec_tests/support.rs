use super::*;

/// The fixture's shortened idle limit. Every test duration that must
/// outlast it is a multiple of it. Windows gets more room because starting
/// a background PowerShell job alone can take longer than 30 ms there.
pub(super) const IDLE: Duration = if cfg!(windows) {
    Duration::from_millis(500)
} else {
    Duration::from_millis(30)
};

/// A command that runs for `IDLE * factor`.
pub(super) fn sleep_command(factor: u32) -> String {
    let millis = (IDLE * factor).as_millis();
    if cfg!(windows) {
        format!("Start-Sleep -Milliseconds {millis}")
    } else {
        format!("sleep {}", millis as f64 / 1000.0)
    }
}

pub(super) struct Fixture {
    pub(super) _root: tempfile::TempDir,
    pub(super) processes: Arc<ProcessTools>,
    pub(super) mode: Arc<CodeMode>,
    pub(super) store: Store,
    pub(super) harness: Harness,
    pub(super) session: String,
}
impl Fixture {
    pub(super) fn new(code: String) -> Self {
        Self::with_config(code, ExecConfig::default())
    }
    pub(super) fn with_config(code: String, config: ExecConfig) -> Self {
        Self::with_config_and_web(code, config, None)
    }
    pub(super) fn with_config_and_web(
        code: String,
        config: ExecConfig,
        web: Option<&WebConfig>,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let processes = Arc::new(
            ProcessTools::with_config(
                WorkspaceTools::new(root.path()).unwrap(),
                config,
                root.path().join("data"),
            )
            .unwrap(),
        );
        let mut registry = ToolRegistry::new();
        processes.register(&mut registry).unwrap();
        processes.register_wait(&mut registry).unwrap();
        registry.register_host(Arc::new(StalledHost)).unwrap();
        if let Some(web) = web {
            register_fetch_search_tools(&mut registry, web).unwrap();
        }
        let artifacts =
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
        let mode = Arc::new(
            CodeMode::new(root.path().to_owned(), artifacts)
                .unwrap()
                .with_idle_limit(IDLE),
        );
        mode.register(&mut registry).unwrap();
        let provider = Arc::new(ScriptedProvider::with_steps(
            vec![ScriptedStep::Calls {
                calls: vec![ToolCall {
                    id: "cell".into(),
                    name: "js".into(),
                    arguments: json!({"code":code}),
                }],
            }],
            "finished",
        ));
        let store = Store::in_memory().unwrap();
        let session = store.create_session("reduced deadline").unwrap().id;
        let harness = Harness::new(
            store.clone(),
            provider,
            registry,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Self {
            _root: root,
            processes,
            mode,
            store,
            harness,
            session,
        }
    }
    pub(super) async fn result(&self) -> bubblepaw_core::Message {
        let run = self
            .harness
            .prompt(&self.session, "execute", RunOptions::default())
            .await
            .unwrap();
        assert_eq!(run.status, "completed", "{:?}", run.error);
        let result = self
            .store
            .messages(&self.session)
            .unwrap()
            .into_iter()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        if result.is_error {
            eprintln!("reduced-idle fixture tool error: {}", result.content);
        }
        result
    }
    pub(super) async fn shutdown(&self) {
        self.mode.shutdown().await;
        self.processes.shutdown().await;
    }
}
