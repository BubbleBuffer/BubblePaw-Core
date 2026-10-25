use super::*;
#[test]
fn invalid_step_result_retains_valid_sdk_quota() {
    let error = decode_step_result(
        json!({"text":"hello","tool_calls":[],"usage":{"input_tokens":"invalid private detail"},"quota_events":[{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.72}]}),
        "claude-work", "work",
    ).unwrap_err();
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.kind, ProviderErrorKind::Fatal);
    assert_eq!(typed.quota.len(), 1);
    assert_eq!(typed.quota[0].provider, "claude-work");
    assert_eq!(typed.quota[0].account, "work");
    assert_eq!(typed.quota[0].used_percent, 72.0);
    assert!(!typed.message.contains("private detail"));
}
#[test]
fn sdk_warning_and_rejection_survive_typed_rpc_error() {
    let error = rpc_error(
        &json!({"message":"raw SDK text must not survive","data":{"kind":"rate_limit","resets_at":"2026-10-21T00:00:00Z","limit_type":"seven_day","quota_events":[{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.72},{"status":"rejected","rateLimitType":"seven_day","resetsAt":1792540800}]}}),
        &[],
    );
    assert_eq!(error.quota.len(), 2);
    assert_eq!(error.quota[0].used_percent, 72.0);
    assert_eq!(error.quota[1].used_percent, 100.0);
    assert_eq!(
        error.quota[1].source,
        bubblepaw_core::QuotaSource::Rejection
    );
    assert!(!error.message.contains("raw SDK"));
}
#[test]
fn terminal_sdk_billing_signal_stays_fatal() {
    let error = rpc_error(
        &json!({"message":"private SDK diagnostic","data":{"kind":"fatal","terminal_quota":true}}),
        &[],
    );
    assert!(super::super::is_terminal_quota_error(&error));
    assert!(!error.retryable());
}
