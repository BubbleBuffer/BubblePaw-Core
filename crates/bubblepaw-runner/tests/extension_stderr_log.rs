//! Extension stderr is logged at warn with secrets redacted.
//!
//! Its own test binary with one test, so the capturing subscriber can be the
//! global default, installed before any extension starts. A thread-local
//! `set_default` raced with parallel tests in a shared binary: tracing caches
//! callsite interest and the max level globally, and the line was sometimes
//! never captured.
use bubblepaw_core::{CancellationToken, HistoryCursor, Message, ModelRequest, Provider, Role};
use bubblepaw_runner::provider::{ExtensionConfig, ExtensionProvider};
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn extension_stderr_is_logged_at_warn_with_secrets_redacted() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();
    let provider = ExtensionProvider::new(ExtensionConfig {
        id: "fake".into(),
        command: env!("CARGO_BIN_EXE_bubblepaw-fake-extension").into(),
        env: [("FAKE_API_TOKEN".into(), "configured-secret-value".into())].into(),
        ..Default::default()
    });
    let request = ModelRequest {
        model: "direct-model".into(),
        session_id: "s".into(),
        history: HistoryCursor {
            epoch: "e1".into(),
            rewritten_from: None,
        },
        messages: vec![Message {
            id: "m1".into(),
            session_id: "s".into(),
            role: Role::User,
            origin: None,
            content: "stderr-exit STDERR_LINE configured-secret-value Bearer bearer-secret".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-08T00:00:00Z".into(),
        }],
        ..Default::default()
    };
    provider
        .generate(request, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    let mut logged = String::new();
    for _ in 0..500 {
        logged = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
        if logged.contains("STDERR_LINE") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(logged.contains(" WARN "), "{logged}");
    assert!(
        logged.contains("extension stderr: STDERR_LINE [redacted] Bearer [redacted]"),
        "{logged}"
    );
    assert!(!logged.contains("configured-secret-value") && !logged.contains("bearer-secret"));
}
