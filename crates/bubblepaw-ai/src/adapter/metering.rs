//! Physical billing and retained-response output-report boundaries.
use crate::usage_sink::{AttemptReport, report_attempt};
use crate::{ModelResponse, ProviderError, Usage};
use anyhow::Result;

/// One physical transport request. A dropped request is recorded as failed;
/// known usage is filled before provider continuation totals are accumulated.
/// Reports go to the usage sink of the active scope and are dropped without one.
pub struct PhysicalRequest(Option<AttemptReport>);
impl PhysicalRequest {
    pub fn new(provider: &str, model: &str) -> Self {
        Self(Some(AttemptReport {
            request_id: crate::id(),
            created_at: chrono::Utc::now().to_rfc3339(),
            account: crate::current_account().unwrap_or_else(|| provider.to_owned()),
            provider: provider.to_owned(),
            model: model.to_owned(),
            usage: Usage::default(),
            failed: false,
            output_reported: false,
        }))
    }
    pub fn name_model(&mut self, model: &str) {
        if let Some(record) = self.0.as_mut() {
            record.model = model.to_owned();
        }
    }
    pub fn finish(&mut self, usage: Option<&Usage>, failed: bool) {
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
            record.output_reported = output_reported;
            report_attempt(record);
        }
    }
    pub fn finish_response(&mut self, response: &ModelResponse) {
        self.finish_with_output_presence(
            response.usage.as_ref(),
            false,
            response_output_reported(response),
        );
    }
    pub fn finish_result(&mut self, result: &Result<ModelResponse>) {
        self.finish_borrowed_result(result.as_ref());
    }
    pub fn finish_borrowed_result(
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
            record.output_reported = false;
            report_attempt(record);
        }
    }
}

/// Whether the provider reported an output count for this response.
pub fn response_output_reported(response: &ModelResponse) -> bool {
    match &response.last_request_usage {
        Some(usage) => usage.output_tokens.is_some(),
        None => response.usage.is_some(),
    }
}
