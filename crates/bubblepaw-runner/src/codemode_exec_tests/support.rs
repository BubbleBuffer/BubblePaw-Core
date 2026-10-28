use super::*;

/// The fixture's shortened idle limit. Every test duration that must
/// outlast it is a multiple of it. Windows gets more room because starting
/// the fixture's explicit PowerShell process can take longer than 30 ms there.
pub(super) const IDLE: Duration = if cfg!(windows) {
    Duration::from_millis(500)
} else {
    Duration::from_millis(30)
};

/// A command that runs for `IDLE * factor`.
pub(super) fn sleep_command(factor: u32) -> Value {
    timed_command(IDLE * factor, "", "")
}

/// Print exact bytes around a delay without adding line endings.
pub(super) fn timed_command(delay: Duration, before: &str, after: &str) -> Value {
    #[cfg(windows)]
    {
        windows_command(delay, before, after, false)
    }
    #[cfg(not(windows))]
    {
        let quote = |text: &str| format!("'{}'", text.replace('\'', "'\"'\"'"));
        json!(format!(
            "printf %s {}; sleep {}; printf %s {}",
            quote(before),
            delay.as_secs_f64(),
            quote(after)
        ))
    }
}

pub(super) fn controlled_poll_command() -> Value {
    #[cfg(windows)]
    {
        windows_command(Duration::ZERO, "ready", "poll-finished", true)
    }
    #[cfg(not(windows))]
    json!("printf ready; while [ ! -e release.txt ]; do sleep 0.02; done; printf poll-finished")
}

#[cfg(windows)]
fn windows_command(delay: Duration, before: &str, after: &str, gate: bool) -> Value {
    // A nonempty sentinel avoids PowerShell 5.1's handling of empty -File args.
    json!({
        "parts":["powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "," "," "," "," ",""],
        "values":["exec-fixture.ps1",delay.as_millis().to_string(),
            if before.is_empty() { "__none__" } else { before },
            if after.is_empty() { "__none__" } else { after },
            if gate { "1" } else { "0" }]
    })
}

#[cfg(windows)]
fn install_exec_fixture(root: &std::path::Path) {
    std::fs::write(root.join("exec-fixture.ps1"), concat!(
        "param([int]$DelayMs, [string]$Before, [string]$After, [int]$Gate)\r\n",
        "if ($Before -ne '__none__') { [Console]::Out.Write($Before) }\r\n",
        "if ($Gate -eq 1) {\r\n",
        "  while (-not (Test-Path -LiteralPath 'release.txt')) { Start-Sleep -Milliseconds 20 }\r\n",
        "} else { Start-Sleep -Milliseconds $DelayMs }\r\n",
        "if ($After -ne '__none__') { [Console]::Out.Write($After) }\r\n"
    )).unwrap();
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
        #[cfg(windows)]
        install_exec_fixture(root.path());
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
