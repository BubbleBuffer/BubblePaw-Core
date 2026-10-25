//! Extension error text: typed kinds keep fixed messages, fatal keeps a
//! redacted line, and child stderr is logged at warn, never in the error.
use super::*;
use std::io::Write;

#[tokio::test]
async fn extension_rpc_error_messages_hide_raw_text_and_keep_option_names() {
    let provider = fake();
    for (command, expected) in [
        (
            "raw-error auth|RAW_RPC_ERROR_SENTINEL",
            "extension authentication failed",
        ),
        (
            "raw-error rate_limit|RAW_RPC_ERROR_SENTINEL",
            "extension rate limit reached",
        ),
        (
            "raw-error context|RAW_RPC_ERROR_SENTINEL",
            "extension context limit exceeded",
        ),
        (
            "raw-error transient|RAW_RPC_ERROR_SENTINEL",
            "extension request failed transiently",
        ),
        (
            "option-error output_schema|RAW_RPC_ERROR_SENTINEL",
            "extension rejected output_schema",
        ),
        (
            "option-error reasoning_effort|RAW_RPC_ERROR_SENTINEL",
            "extension rejected reasoning_effort",
        ),
        ("raw-error transient|cancelled", "cancelled"),
    ] {
        let error = call(&provider, ext_step("direct-model", command, "e1", None))
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert_eq!(typed.message, expected);
        assert!(!format!("{error:#}").contains("RAW_RPC_ERROR_SENTINEL"));
        if command.contains("rate_limit") {
            assert_eq!(
                typed.kind,
                ProviderErrorKind::RateLimit {
                    resets_at: Some("2026-10-08T14:00:00Z".into()),
                }
            );
        }
    }
}

#[tokio::test]
async fn extension_process_errors_do_not_include_child_stderr() {
    let provider = fake();
    let error = call(
        &provider,
        ext_step(
            "direct-model",
            "stderr-exit RAW_STDERR_SENTINEL",
            "e1",
            None,
        ),
    )
    .await
    .unwrap_err();
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.kind, ProviderErrorKind::Transient);
    assert_eq!(typed.message, "extension fake exited");
    assert!(!format!("{error:#}").contains("RAW_STDERR_SENTINEL"));
}

#[tokio::test]
async fn extension_fatal_error_keeps_a_redacted_first_line() {
    let provider = fake_with_env("fake", &[("FAKE_API_TOKEN", "configured-secret-value")]);
    let error = call(
        &provider,
        ext_step(
            "direct-model",
            "raw-error fatal|Unknown option --foo configured-secret-value Bearer bearer-secret ANTHROPIC_API_KEY=env-secret",
            "e1",
            None,
        ),
    )
    .await
    .unwrap_err();
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.kind, ProviderErrorKind::Fatal);
    assert_eq!(
        typed.message,
        "extension request failed: Unknown option --foo [redacted] Bearer [redacted] ANTHROPIC_API_KEY=[redacted]"
    );
    let empty = call(
        &provider,
        ext_step("direct-model", "raw-error fatal|", "e1", None),
    )
    .await
    .unwrap_err();
    assert_eq!(
        ProviderError::of(&empty).unwrap().message,
        "extension request failed"
    );
}

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
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let provider = fake_with_env("fake", &[("FAKE_API_TOKEN", "configured-secret-value")]);
    let _ = call(
        &provider,
        ext_step(
            "direct-model",
            "stderr-exit STDERR_LINE configured-secret-value Bearer bearer-secret",
            "e1",
            None,
        ),
    )
    .await
    .unwrap_err();
    let mut logged = String::new();
    for _ in 0..200 {
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
