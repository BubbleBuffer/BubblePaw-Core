use super::*;

#[tokio::test]
async fn a_run_cancelled_before_its_model_call_records_one_attempt_and_calls_no_model() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(BlockingProvider {
        started: Arc::new(Notify::new()),
        calls: calls.clone(),
    });
    let f = fixture_with_provider(&workspace, provider);
    let db = root.path().join("delegations.db");
    let mut ledger = Ledger::open(&db).unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let result = delegate::run_with_control(
        f.harness.clone(),
        &f.store,
        &mut ledger,
        cancel_request(&workspace, assignment(1), &f.collector),
        token,
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.status, "failed", "{result:#?}");
    assert_eq!(
        result.error.as_deref(),
        Some("cancelled before the model run")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        attempt_rows(&db),
        vec![("cancelled".to_owned(), "failed".to_owned(), 0)]
    );
}
