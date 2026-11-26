//! Retained-response output-report boundary of the registry's own fallbacks.
use anyhow::Result;
use bubblepaw_ai::adapter::response_output_reported;
use bubblepaw_core::{ModelResponse, ProviderError, RequestUsage};

pub(super) fn record_model_result(mut record: RequestUsage, result: &Result<ModelResponse>) {
    let output_reported = result.as_ref().is_ok_and(response_output_reported);
    record.failed = result.is_err();
    record.usage = result
        .as_ref()
        .ok()
        .and_then(|response| response.usage.clone())
        .or_else(|| {
            result
                .as_ref()
                .err()
                .and_then(ProviderError::of)
                .and_then(|error| error.usage.clone())
        })
        .unwrap_or_default();
    bubblepaw_core::record_request_usage_with_output_presence(record, output_reported);
}
