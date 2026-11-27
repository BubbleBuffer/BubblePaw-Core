//! What the router leaves to its host: billing, quota, accounts and logging.
//!
//! The router knows routes, attempts and usage totals. A host that bills or waits
//! on quota implements [`RouteObserver`] and the router calls it around each
//! candidate and each attempt. [`NoObserver`] does nothing, so a router works
//! without a host.
use crate::{CancellationToken, ModelResponse};
use anyhow::Result;
use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The provider id and forwarded model name of the route being tried.
#[derive(Clone, Copy, Debug)]
pub struct RouteRef<'a> {
    pub provider: &'a str,
    pub model: &'a str,
}

/// One candidate's whole retry loop, as the router hands it to the observer.
pub type RetryLoop<'a> = dyn FnMut() -> BoxFuture<'a, Result<ModelResponse>> + Send + 'a;

pub trait RouteObserver: Send + Sync {
    /// Whether rate limits go to the observer instead of the retry loop (set
    /// `RetryPolicy::yield_rate_limits`). True exactly when `candidate` drives the
    /// loop with a quota policy.
    fn yield_rate_limits(&self) -> bool {
        false
    }
    /// Wraps one candidate's retry loop. Scopes the observer needs (account,
    /// provider) are entered here, and a host that waits between attempts calls
    /// `retry` again after each wait. The default runs it once.
    fn candidate<'a>(
        &'a self,
        _route: RouteRef<'a>,
        _cancellation: &'a CancellationToken,
        retry: &'a mut RetryLoop<'a>,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move { retry().await })
    }
    /// Wraps one provider attempt, inside `candidate`: `run` is the provider call
    /// with its publication marking and thinking retraction applied.
    fn attempt<'a>(
        &'a self,
        _route: RouteRef<'a>,
        run: BoxFuture<'a, Result<ModelResponse>>,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        run
    }
    /// A candidate route failed without ending the fallback chain.
    fn route_failed(&self, _route: &str, _reason: &str) {}
}

/// The observer of a router with no host: no scopes, no waiting, no logging.
pub struct NoObserver;
impl RouteObserver for NoObserver {}
