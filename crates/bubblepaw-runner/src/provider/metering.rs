//! Physical billing and retained-response output-report boundaries.
use super::*;
use bubblepaw_core::RequestUsage;

/// One physical transport request. A dropped request is recorded as failed;
/// known usage is filled before provider continuation totals are accumulated.
pub(crate) struct PhysicalRequest(Option<bubblepaw_core::RequestUsage>);
impl PhysicalRequest {
    pub(crate) fn new(provider: &str, model: &str) -> Self {
        Self(Some(bubblepaw_core::RequestUsage::new(provider, model)))
    }
    pub(crate) fn name_model(&mut self, model: &str) {
        if let Some(record) = self.0.as_mut() {
            record.model = model.to_owned();
        }
    }
    pub(crate) fn finish(&mut self, usage: Option<&Usage>, failed: bool) {
        self.finish_with_output_presence(usage, failed, usage.is_some());
    }
    fn finish_with_output_presence(
        &mut self,
        usage: Option<&Usage>,
        failed: bool,
        output_reported: bool,
    ) {
        if let Some(mut record) = self.0.take() {
            record.usage = usage.cloned().unwrap_or_default();
            record.failed = failed;
            bubblepaw_core::record_request_usage_with_output_presence(record, output_reported);
        }
    }
    pub(crate) fn finish_response(&mut self, response: &ModelResponse) {
        self.finish_with_output_presence(
            response.usage.as_ref(),
            false,
            response_output_reported(response),
        );
    }
    pub(crate) fn finish_result(&mut self, result: &Result<ModelResponse>) {
        self.finish_borrowed_result(result.as_ref());
    }
    pub(crate) fn finish_borrowed_result(
        &mut self,
        result: std::result::Result<&ModelResponse, &anyhow::Error>,
    ) {
        match result {
            Ok(response) => self.finish_response(response),
            Err(error) => self.finish(
                ProviderError::of(error).and_then(|error| error.usage.as_ref()),
                true,
            ),
        }
    }
}
impl Drop for PhysicalRequest {
    fn drop(&mut self) {
        if let Some(mut record) = self.0.take() {
            record.failed = true;
            bubblepaw_core::record_request_usage_with_output_presence(record, false);
        }
    }
}

fn response_output_reported(response: &ModelResponse) -> bool {
    match &response.last_request_usage {
        Some(usage) => usage.output_tokens.is_some(),
        None => response.usage.is_some(),
    }
}

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
