//! Physical output accounting and rejection boundaries.
use super::*;

/// Native adapters meter each round before joining responses or doing final postprocessing.
struct MeteredRounds {
    outputs: Vec<Option<u64>>,
    reject_after_metering: bool,
}

#[async_trait]
impl Provider for MeteredRounds {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let mut total = Usage::default();
        for output in &self.outputs {
            // Match native parsing: Usage exists with zero even when its output field was omitted.
            let response = ModelResponse {
                text: "partial reply".into(),
                usage: Some(Usage {
                    output_tokens: output.unwrap_or(0),
                    ..Default::default()
                }),
                last_request_usage: Some(LastRequestUsage {
                    input_tokens: None,
                    output_tokens: *output,
                }),
                ..Default::default()
            };
            let mut meter = crate::provider::PhysicalRequest::new("adapter", &request.model);
            meter.finish_response(&response);
            total.add(response.usage.as_ref().unwrap());
        }
        if self.reject_after_metering {
            return Err(ProviderError::new(
                ProviderErrorKind::Auth,
                "postprocessing rejected the reply",
            )
            .into());
        }
        Ok(ModelResponse {
            text: "joined reply".into(),
            usage: Some(total),
            last_request_usage: Some(LastRequestUsage {
                input_tokens: None,
                output_tokens: self.outputs.last().copied().flatten(),
            }),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn registry_discards_physically_successful_rejected_reply_before_fallback() -> Result<()> {
    use bubblepaw_core::{Harness, HarnessConfig, RunOptions, ToolRegistry};
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("metered fallback")?;
    let mut registry = ProviderRegistry::new(
        "primary",
        Arc::new(MeteredRounds {
            outputs: vec![Some(11)],
            reject_after_metering: true,
        }),
    )
    .with_retry_policy(RetryPolicy::once());
    registry.insert(
        "fallback",
        Arc::new(MeteredRounds {
            outputs: vec![Some(7)],
            reject_after_metering: false,
        }),
    )?;
    registry.set_model_fallbacks("primary/model".into(), vec!["fallback/model".into()])?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(registry),
        ToolRegistry::new(),
        HarnessConfig {
            model: "primary/model".into(),
            ..Default::default()
        },
    );
    let before = store.storage_stats()?.chat_transaction_count;
    let run = harness
        .prompt(&session.id, "hello", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed");
    assert_eq!(store.storage_stats()?.chat_transaction_count - before, 2);
    let facts = store.reported_message_tokens(&session.id, 0..=i64::MAX)?;
    assert_eq!(facts.len(), 2);
    assert_eq!(facts[1].tokens, Some(7));
    let billing = store.request_usage()?;
    assert_eq!(billing.len(), 2);
    assert!(billing.iter().all(|row| !row.failed));
    assert!(
        billing
            .iter()
            .any(|row| row.provider == "primary" && row.usage.output_tokens == 11)
    );
    assert!(
        billing
            .iter()
            .any(|row| row.provider == "fallback" && row.usage.output_tokens == 7)
    );
    Ok(())
}

#[tokio::test]
async fn physical_round_metering_preserves_omission_and_explicit_zero_through_registry()
-> Result<()> {
    use bubblepaw_core::{Harness, HarnessConfig, RunOptions, ToolRegistry};
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    for (outputs, expected) in [
        (vec![None, Some(7)], None),
        (vec![Some(0), Some(7)], Some(7)),
        (vec![Some(0), Some(0)], Some(0)),
    ] {
        let session = store.create_session("metered continuation")?;
        let registry = ProviderRegistry::new(
            "native",
            Arc::new(MeteredRounds {
                outputs,
                reject_after_metering: false,
            }),
        )
        .with_retry_policy(RetryPolicy::once());
        let harness = Harness::new(
            store.clone(),
            Arc::new(registry),
            ToolRegistry::new(),
            HarnessConfig {
                model: "native/model".into(),
                ..Default::default()
            },
        );
        let run = harness
            .prompt(&session.id, "hello", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed");
        let facts = store.reported_message_tokens(&session.id, 0..=i64::MAX)?;
        assert_eq!(facts[1].tokens, expected);
        let billing: Vec<_> = store
            .request_usage()?
            .into_iter()
            .filter(|row| row.session_id == session.id)
            .collect();
        assert_eq!(billing.len(), 2);
        assert!(billing.iter().all(|row| !row.failed));
    }
    Ok(())
}
