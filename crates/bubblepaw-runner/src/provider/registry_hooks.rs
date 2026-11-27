//! What the registry hands the router: the account and provider scopes, the quota
//! host, per-attempt usage capture and the route-failure log.
use super::{ProviderRegistry, record_model_result};
use anyhow::Result;
use bubblepaw_ai::{BoxFuture, RetryLoop, RouteObserver, RouteRef};
use bubblepaw_core::{CancellationToken, ModelResponse, RequestUsage};

pub(super) struct Hooks<'r> {
    registry: &'r ProviderRegistry,
}
impl<'r> Hooks<'r> {
    pub(super) fn new(registry: &'r ProviderRegistry) -> Self {
        Self { registry }
    }
}

impl RouteObserver for Hooks<'_> {
    /// A quota host drives the retry loop, so it takes the rate limits.
    fn yield_rate_limits(&self) -> bool {
        self.registry.quota_host.lock().unwrap().is_some()
    }
    /// One nesting order for main turns and forks: account scope, provider scope,
    /// the quota host, then the retry loop. The quota host's policy scope covers
    /// every attempt, and rate-limit attempts are not multiplied by the
    /// transport policy.
    fn candidate<'a>(
        &'a self,
        route: RouteRef<'a>,
        cancellation: &'a CancellationToken,
        retry: &'a mut RetryLoop<'a>,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move {
            let account = self.registry.account(route.provider);
            bubblepaw_core::with_request_usage_account(
                &account,
                bubblepaw_core::with_request_usage_provider(
                    route.provider,
                    self.registry.quota_request(
                        route.provider,
                        &account,
                        route.model,
                        cancellation.clone(),
                        retry,
                    ),
                ),
            )
            .await
        })
    }
    /// Count the usage records the attempt wrote; when it wrote none, record the
    /// result as the billing fallback, and drop the records of a failed attempt.
    fn attempt<'a>(
        &'a self,
        route: RouteRef<'a>,
        run: BoxFuture<'a, Result<ModelResponse>>,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move {
            let record = RequestUsage::new(route.provider, route.model);
            let before = bubblepaw_core::request_usage_count();
            let result = run.await;
            if bubblepaw_core::request_usage_count() == before {
                record_model_result(record, &result);
            }
            if result.is_err()
                && let Some(start) = before
            {
                bubblepaw_core::discard_request_usage_since(start);
            }
            result
        })
    }
    fn route_failed(&self, route: &str, reason: &str) {
        tracing::warn!(target: "bubblepaw::provider", route = %route, reason = %reason, "model route failed");
    }
}
