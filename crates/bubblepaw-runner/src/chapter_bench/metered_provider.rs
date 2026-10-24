//! Meter provider invocations, including repeated decision samples and registry retries.
use super::{Arm, meter::Meter};
use crate::provider::{ProviderRegistry, decision_error_usage};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Capabilities, DecisionProvider, DecisionRequest, DecisionResponse,
    ModelRequest, ModelResponse, Provider, ProviderError, TextCallback, WireShape,
};
use serde_json::Value;
use std::{sync::Arc, time::Instant};

pub(super) fn wrap(
    source: Arc<ProviderRegistry>,
    meter: Arc<Meter>,
    arms: &[Arm],
) -> Result<Arc<ProviderRegistry>> {
    ensure!(
        source.llm_available(super::SUMMARY_MODEL),
        "Missing episode summary model route"
    );
    for arm in arms {
        ensure!(
            source.decision_available(&arm.model),
            "Unavailable model route for arm {}: {}",
            arm.name,
            arm.model
        );
    }
    let registry = source.map_benchmark_transports(
        |route, inner| {
            Ok(Arc::new(Metered {
                inner,
                route: route.into(),
                meter: meter.clone(),
            }))
        },
        |route, inner| {
            Ok(Arc::new(MeteredDecision {
                inner,
                route: route.into(),
                meter: meter.clone(),
            }))
        },
    )?;
    Ok(Arc::new(registry))
}
struct Metered {
    inner: Arc<dyn Provider>,
    route: String,
    meter: Arc<Meter>,
}
impl Metered {
    async fn request(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
        decision: bool,
    ) -> Result<ModelResponse> {
        let model = format!("{}/{}", self.route, request.model);
        let call = self.meter.begin(&model)?;
        let now = Instant::now();
        let result = if decision {
            self.inner
                .generate_decision(request, on_text, cancellation)
                .await
        } else {
            self.inner.generate(request, on_text, cancellation).await
        };
        let usage = match &result {
            Ok(r) => r.usage.clone(),
            Err(e) => ProviderError::of(e).and_then(|e| e.usage.clone()),
        };
        self.meter.finish(
            call,
            usage,
            None,
            now.elapsed().as_secs_f64() * 1000.0,
            result.is_err(),
        )?;
        result
    }
}
#[async_trait]
impl Provider for Metered {
    async fn generate(
        &self,
        r: ModelRequest,
        t: TextCallback,
        c: CancellationToken,
    ) -> Result<ModelResponse> {
        self.request(r, t, c, false).await
    }
    async fn generate_decision(
        &self,
        r: ModelRequest,
        t: TextCallback,
        c: CancellationToken,
    ) -> Result<ModelResponse> {
        self.request(r, t, c, true).await
    }
    fn capabilities(&self, model: &str) -> Capabilities {
        self.inner.capabilities(model)
    }
    fn estimate_input_tokens(
        &self,
        r: &ModelRequest,
        c: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        self.inner.estimate_input_tokens(r, c)
    }
    fn wire_shape(&self, r: &ModelRequest) -> Option<WireShape> {
        self.inner.wire_shape(r)
    }
    async fn close_session(&self, id: &str) -> Result<()> {
        self.inner.close_session(id).await
    }
}
struct MeteredDecision {
    inner: Arc<dyn DecisionProvider>,
    route: String,
    meter: Arc<Meter>,
}
#[async_trait]
impl DecisionProvider for MeteredDecision {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        let model = format!("{}/{}", self.route, request.model);
        let call = self.meter.begin(&model)?;
        let now = Instant::now();
        let result = self.inner.decide(request, cancellation).await;
        let usage = match &result {
            Ok(r) => Some(r.usage.clone()),
            Err(e) => decision_error_usage(e).cloned(),
        };
        let cost = usage.as_ref().and_then(|u| u.cost_usd);
        self.meter.finish(
            call,
            usage.map(Into::into),
            cost,
            now.elapsed().as_secs_f64() * 1000.0,
            result.is_err(),
        )?;
        result
    }
}
