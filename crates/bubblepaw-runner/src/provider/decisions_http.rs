//! JSON HTTP send/read boundary used by decision adapters.
use super::*;

pub(in crate::provider) async fn post_json(
    client: &reqwest::Client,
    endpoint: &str,
    key: &str,
    body: &Value,
    session_header: &str,
    session: &str,
    cancellation: CancellationToken,
) -> Result<Value> {
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let dump = crate::provider::http_dump::HttpDump::begin(body, "decider");
    let mut response = tokio::select! { result=client.post(endpoint).bearer_auth(key).header("User-Agent","bubblepaw/lean-rust").header(session_header,session).json(body).send()=>result.map_err(|_|classify(ProviderErrorKind::Transient,anyhow!("Episode model transport failed")))?, _=cancellation.cancelled()=>return Err(cancelled()) };
    if let Some(dump) = dump {
        dump.attach(&mut response);
    }
    if !response.status().is_success() {
        let code = response.status().as_u16();
        let retry_after = crate::provider::http_error::retry_after(response.headers());
        let bytes = crate::provider::http_error::read_body(response, &cancellation).await?;
        // Episode decisions have no context-overflow classification of their own.
        let kind =
            crate::provider::http_error::compatible_status_kind(code, retry_after, false, &bytes);
        let error = if let Some(reason) = crate::provider::http_error::terminal_reason(&bytes) {
            crate::provider::terminal_quota_failure(
                reason,
                crate::provider::http_error::diagnostic("Episode model", code, &bytes, &[key]),
            )
        } else {
            crate::provider::attach_retry_after(
                classify(
                    kind,
                    anyhow!(crate::provider::http_error::diagnostic(
                        "Episode model",
                        code,
                        &bytes,
                        &[key]
                    )),
                ),
                code,
                retry_after,
            )
        };
        let error = crate::provider::http_error::mark_bare_bad_request(error, code, &bytes);
        return Err(match serde_json::from_slice::<Value>(&bytes) {
            Ok(payload) if payload.get("usage").is_some_and(Value::is_object) => {
                let reported = physical_usage(&payload);
                let mut source = bubblepaw_core::ProviderError::of(&error)
                    .cloned()
                    .unwrap_or_else(|| {
                        bubblepaw_core::ProviderError::new(
                            ProviderErrorKind::Fatal,
                            error.to_string(),
                        )
                    });
                source.usage = Some(reported.clone());
                let metadata = DecisionParseError {
                    message: error.to_string(),
                    usage: DecisionUsage::from(reported),
                };
                error.context(source).context(metadata)
            }
            _ => error,
        });
    }
    let bytes = if crate::provider::http_dump::enabled(&response) {
        crate::provider::stream::read_body(response, &cancellation).await?
    } else {
        tokio::select! { result=response.bytes()=>result.map_err(|_|classify(ProviderErrorKind::Transient,anyhow!("Episode model response transport failed")))?.to_vec(), _=cancellation.cancelled()=>return Err(cancelled()) }
    };
    serde_json::from_slice(&bytes).map_err(|_| {
        classify(
            ProviderErrorKind::Fatal,
            anyhow!("Episode model response is not JSON"),
        )
    })
}
