//! A provider rejection of a main request as too long compacts once and sends
//! the request again; a second rejection ends the run with the provider's error.
use super::*;

#[tokio::test]
async fn provider_overflow_compacts_once_and_retries_the_request() -> Result<()> {
    let fixture = Fixture::new(0).await?;
    let gathers_before = fixture.models.inputs.lock().unwrap().len();
    fixture.provider.overflows.store(1, Ordering::SeqCst);
    let run = fixture.run(PROMPT).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let requests = fixture.provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3, "rejected, retried, then the final round");
    assert!(
        fixture.policy.estimated_tokens(&requests[1])
            < fixture.policy.estimated_tokens(&requests[0]),
        "the retry carries a smaller context"
    );
    assert!(fixture.models.inputs.lock().unwrap().len() > gathers_before);
    let covered = fixture.overhead.history_frontier(&fixture.session)?;
    assert!(covered.unwrap_or(0) > fixture.covered_before.unwrap_or(0));
    Ok(())
}

#[tokio::test]
async fn second_provider_overflow_ends_the_run_with_the_provider_error() -> Result<()> {
    let fixture = Fixture::new(0).await?;
    fixture.provider.overflows.store(2, Ordering::SeqCst);
    let run = fixture.run(PROMPT).await?;
    assert_eq!(run.status, "failed");
    assert!(run.error.unwrap_or_default().contains("prompt is too long"));
    assert_eq!(fixture.provider.requests.lock().unwrap().len(), 2);
    assert_eq!(fixture.bulky.invocations.load(Ordering::SeqCst), 0);
    Ok(())
}
