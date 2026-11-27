//! The quota host drives the transport retry loop on main turns, as on forks,
//! so rate-limit attempts are not multiplied by the registry retry policy.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind,
    Store, StreamCallback,
};
use bubblepaw_runner::{
    provider::{ProviderRegistry, RetryPolicy},
    quota::QuotaHost,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct AlwaysRateLimited(Arc<AtomicUsize>);

#[async_trait]
impl Provider for AlwaysRateLimited {
    async fn generate(
        &self,
        _: ModelRequest,
        _: StreamCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ProviderError::new(ProviderErrorKind::RateLimit { resets_at: None }, "rejected").into())
    }
}

#[tokio::test]
async fn rate_limited_main_turn_makes_only_the_quota_hosts_attempts() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let store = Store::in_memory()?;
    let mut registry = ProviderRegistry::new("p", Arc::new(AlwaysRateLimited(calls.clone())));
    registry.set_account("p", "shared");
    registry.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_secs(1),
        yield_rate_limits: false,
    });
    registry.set_quota_host(Arc::new(QuotaHost::new(store, Duration::ZERO)?));
    let request = ModelRequest {
        model: "p/model".into(),
        ..Default::default()
    };
    let error = registry
        .generate(request, Arc::new(|_| {}), CancellationToken::new())
        .await
        .expect_err("every attempt is rate limited");
    assert!(matches!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::RateLimit { .. }
    ));
    // Four short quota attempts; the transport policy (3) does not repeat them.
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    Ok(())
}

struct Replies(&'static str);

#[async_trait]
impl Provider for Replies {
    async fn generate(
        &self,
        _: ModelRequest,
        _: StreamCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: self.0.into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn quota_attempts_on_one_route_come_before_the_next_fallback() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new("p", Arc::new(AlwaysRateLimited(calls.clone())));
    registry.insert("q", Arc::new(Replies("from q")))?;
    registry.set_model_fallbacks("p/model".into(), vec!["q/model".into()])?;
    registry.set_quota_host(Arc::new(QuotaHost::new(
        Store::in_memory()?,
        Duration::ZERO,
    )?));
    let request = ModelRequest {
        model: "p/model".into(),
        ..Default::default()
    };
    let reply = registry
        .generate(request, Arc::new(|_| {}), CancellationToken::new())
        .await?;
    // The first route spends the quota host's four attempts, then the fallback serves.
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(reply.text, "from q");
    assert_eq!(reply.route.unwrap().selected, "q/model");
    Ok(())
}
