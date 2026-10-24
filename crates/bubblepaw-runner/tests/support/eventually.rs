//! Condition waits shared by runner integration tests, in place of fixed sleeps.
#![allow(dead_code)]

use std::time::Duration;

/// Generous bound for a condition that normally holds within milliseconds.
pub fn wait_timeout() -> Duration {
    Duration::from_secs(if cfg!(windows) { 30 } else { 10 })
}

/// Poll `ready` every 10 ms until it holds; panic with `what` after `wait_timeout()`.
pub async fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    let waited = tokio::time::timeout(wait_timeout(), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

/// Blocking form for synchronous tests.
pub fn eventually_blocking(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + wait_timeout();
    while !ready() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
