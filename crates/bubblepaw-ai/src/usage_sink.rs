//! Where adapters report each physical request, without knowing who records it.
use crate::{RequestKind, Usage};
use std::{future::Future, sync::Arc};

/// One physical request as an adapter saw it. Identity and account are fixed
/// when the request starts, so a failure keeps them.
#[derive(Clone, Debug)]
pub struct AttemptReport {
    pub request_id: String,
    pub created_at: String,
    pub account: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub failed: bool,
    pub output_reported: bool,
}

/// The host side of request metering: records reports and names the request role.
pub trait UsageSink: Send + Sync {
    fn report(&self, report: AttemptReport);
    fn request_kind(&self) -> RequestKind;
}

tokio::task_local! { static SINK: Arc<dyn UsageSink>; }

/// Route adapter reports inside `future` to `sink`. Without a sink they are dropped.
pub async fn with_usage_sink<F: Future>(sink: Arc<dyn UsageSink>, future: F) -> F::Output {
    SINK.scope(sink, future).await
}

pub(crate) fn report_attempt(report: AttemptReport) {
    let _ = SINK.try_with(|sink| sink.report(report));
}

/// Request role of the active sink, or None outside one.
pub fn current_request_kind() -> Option<RequestKind> {
    SINK.try_with(|sink| sink.request_kind()).ok()
}
