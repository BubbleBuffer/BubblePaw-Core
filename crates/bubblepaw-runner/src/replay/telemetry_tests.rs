use super::*;

#[test]
fn intentional_decision_repetitions_are_separate_requests_without_retries() {
    let mut meter = Telemetry::default();
    meter.begin_operation("decision");
    for slot in 1..=3 {
        assert!(!meter.observe_attempt(true));
        assert_eq!(meter.request_slot, slot);
        assert_eq!(meter.physical_call_index, slot);
        assert_eq!(meter.http_attempt, 1);
    }
    assert_eq!(meter.retries, 0);
    assert_eq!(meter.failures, 0);
}

#[test]
fn transport_retry_follows_failure_and_does_not_cross_operation_boundary() {
    let mut meter = Telemetry::default();
    meter.begin_operation("decision");
    assert!(!meter.observe_attempt(false));
    assert!(meter.observe_attempt(true));
    assert_eq!(meter.request_slot, 1);
    assert_eq!(meter.http_attempt, 2);
    assert_eq!(meter.retries, 1);
    assert!(!meter.observe_attempt(true));
    assert_eq!(meter.request_slot, 2);
    assert_eq!(meter.http_attempt, 1);
    assert!(!meter.observe_attempt(false));
    meter.begin_operation("summary");
    assert!(!meter.observe_attempt(true));
    assert_eq!(meter.physical_call_index, 1);
    assert_eq!(meter.request_slot, 1);
    assert_eq!(meter.http_attempt, 1);
    assert_eq!(meter.retries, 1);
    assert_eq!(meter.failures, 2);
}

#[test]
fn guard_blocks_measured_unknown_and_failure_stops() {
    let mut meter = Telemetry::default();
    assert!(meter.guard().is_ok());
    meter.flash_cost_usd = 0.49;
    meter.reserved_unknown_cost_usd = 0.02;
    assert!(meter.guard().is_err());
    meter.reserved_unknown_cost_usd = 0.0;
    meter.jev_cost_usd = 0.01;
    assert!(meter.guard().is_err());
    meter.flash_cost_usd = 0.0;
    meter.jev_cost_usd = 0.0;
    meter.stopped = Some("Three consecutive model failures".into());
    assert!(meter.guard().is_err());
}
#[test]
fn guard_reserves_the_next_call_before_dispatch_including_unknown_spend() {
    let mut meter = Telemetry {
        max_cost_usd: 0.10,
        flash_cost_usd: 0.03,
        reserved_unknown_cost_usd: 0.02,
        ..Default::default()
    };
    assert!(meter.reserve_next(0.04));
    assert!(!meter.reserve_next(0.06));
    assert!(meter.stopped.as_deref().unwrap().contains("0.100000"));
    assert_eq!(meter.flash_cost_usd, 0.03);
    assert!(meter.calls.is_empty());
}
