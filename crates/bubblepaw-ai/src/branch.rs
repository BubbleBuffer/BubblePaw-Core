//! The optional branch ability of a provider: an isolated side call that keeps
//! the main request's native prefix and generation settings.
use crate::{CancellationToken, ModelRequest, ModelResponse, Provider};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// What a branch reports about one request. Each half has its own result so
/// that a caller needing only one is not affected by the other failing.
pub struct BranchView {
    /// Canonical source IDs mapped to the actual rendered input positions.
    /// A missing projection cannot be silently replaced with bounded summary input.
    pub mapping: Result<Option<Value>>,
    /// Token estimate of this one route, without other main-request fallbacks.
    pub estimate: Result<Option<usize>>,
}

impl BranchView {
    /// The view `provider` gives of `request`; a provider that cannot branch
    /// has no mapping and its ordinary input estimate.
    pub fn of(
        provider: &dyn Provider,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Self {
        match provider.branch(&request.model) {
            Some(branch) => branch.view(request, count),
            None => Self {
                mapping: Ok(None),
                estimate: provider.estimate_input_tokens(request, count),
            },
        }
    }
}

#[async_trait]
pub trait Branch: Send + Sync {
    /// Ordered, explicitly configured routes for an isolated branch. This only
    /// describes candidates: it must not update main routing or dispatch a
    /// request. The captured successful main route comes first.
    fn routes(&self, model: &str) -> Vec<String> {
        vec![model.to_owned()]
    }
    /// The source mapping and the token estimate of `request`.
    fn view(&self, request: &ModelRequest, count: &dyn Fn(&Value) -> usize) -> BranchView;
    /// Dispatch an isolated branch. Never update a main mirror or route
    /// selection. Dispatch exactly one route; the caller owns explicit retries
    /// across `routes` while retaining the branch input shape.
    async fn generate_branch(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse>;
}
