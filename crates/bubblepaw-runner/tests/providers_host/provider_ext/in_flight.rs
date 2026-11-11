//! Cancelling and crashing the fake extension while a step is in flight.
use super::*;

/// Waits until the fake extension has logged a received step whose request
/// contains `needle` (`BP_FAKE_STEP_LOG` writes it before the step runs).
async fn step_received(log: &std::path::Path, needle: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .any(|line| line.contains(needle))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fake extension never received {needle:?}"));
}

#[tokio::test]
async fn extension_cancel_returns_the_extension_error() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("steps.jsonl");
    let provider = Arc::new(fake_with_env(
        "fake",
        &[("BP_FAKE_STEP_LOG", log.to_str().unwrap())],
    ));
    provider.start().await.unwrap();
    let token = CancellationToken::new();
    let task = {
        let provider = provider.clone();
        let token = token.clone();
        tokio::spawn(async move {
            provider
                .generate(
                    ext_step("direct-model", "hang", "e1", None),
                    Arc::new(|_| {}),
                    token,
                )
                .await
        })
    };
    step_received(&log, "\"hang\"").await;
    token.cancel();
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.kind, ProviderErrorKind::Transient);
    assert_eq!(typed.message, "cancelled");
    // The extension keeps serving.
    assert_eq!(
        call(&provider, ext_step("direct-model", "x", "e1", None))
            .await
            .unwrap()
            .text,
        "ok"
    );
}

#[tokio::test]
async fn extension_crash_fails_in_flight_steps_and_respawns_with_a_fresh_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("steps.jsonl");
    let provider = Arc::new(fake_with_env(
        "fake",
        &[("BP_FAKE_STEP_LOG", log.to_str().unwrap())],
    ));
    let before = call(&provider, ext_step("direct-model", "x", "e1", None))
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert_eq!(before["history"]["epoch"], "e1");
    let slow = {
        let provider = provider.clone();
        tokio::spawn(async move {
            call(
                &provider,
                ext_step("direct-model", "sleep 2000", "e1", None),
            )
            .await
        })
    };
    step_received(&log, "\"sleep 2000\"").await;
    let error = call(&provider, ext_step("direct-model", "exit", "e1", None))
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Transient
    );
    let error = slow.await.unwrap().unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Transient
    );
    // Respawned: same harness cursor, but a new epoch and no reusable prefix.
    let after = call(&provider, ext_step("direct-model", "x", "e1", Some("m3")))
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert_ne!(after["pid"], before["pid"]);
    assert_ne!(after["history"]["epoch"], "e1");
    assert_eq!(after["history"]["rewritten_from"], Value::Null);
    // The next append keeps the substituted epoch.
    let next = call(&provider, ext_step("direct-model", "x", "e1", None))
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert_eq!(next["history"]["epoch"], after["history"]["epoch"]);
}
