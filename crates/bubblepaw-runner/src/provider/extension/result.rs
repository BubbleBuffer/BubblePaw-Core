use super::*;

pub(super) fn decode_step_result(
    mut value: Value,
    provider: &str,
    account: &str,
) -> Result<ModelResponse> {
    let quota = extension_quota(Some(&value), provider, account);
    if let Some(object) = value.as_object_mut() {
        object.remove("quota_events");
    }
    let mut response: ModelResponse = serde_json::from_value(value).map_err(|_| {
        crate::quota_sources::with_error_quota(
            ProviderError::new(
                ProviderErrorKind::Fatal,
                format!("extension {provider} sent an invalid step result"),
            )
            .into(),
            quota.clone(),
        )
    })?;
    response.quota.extend(quota);
    Ok(response)
}

pub(super) fn validate_calls(
    response: ModelResponse,
    request: &ModelRequest,
    provider: &str,
) -> Result<ModelResponse> {
    if response
        .tool_calls
        .iter()
        .any(|call| !super::super::tool_wire::accepts_request_call(request, &call.name))
    {
        return Err(crate::quota_sources::with_error_quota(
            super::super::decisions::metered_model_error(
                anyhow::anyhow!("extension {provider} returned an unavailable tool"),
                response.usage.clone(),
            ),
            response.quota,
        ));
    }
    Ok(response)
}
