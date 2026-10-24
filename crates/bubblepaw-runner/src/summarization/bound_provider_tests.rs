use super::*;

struct Projection(usize);
#[async_trait]
impl Provider for Projection {
    fn estimate_input_tokens(
        &self,
        _: &ModelRequest,
        _: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Ok(Some(self.0))
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: bubblepaw_core::TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        anyhow::bail!("local projection fixture must not make a model call")
    }
}

#[tokio::test]
async fn bound_preparation_shares_pending_meter_calibration_and_token_cache() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("bound estimator")?.id;
    let message = SummarizingContext::synthetic(&session, Role::User, "task".into());
    store.append_batch(std::slice::from_ref(&message))?;
    let context = Arc::new(SummarizingContext::new(
        store.clone(),
        Arc::new(Projection(1)),
        SummaryConfig::default(),
        None,
    )?);
    let clone = context.as_ref().clone();
    let wrapper = crate::usage::UsageContext::new(store, context.clone());
    assert!(Arc::ptr_eq(&context.worker, &clone.worker));
    assert!(Arc::ptr_eq(&context.meters, &clone.meters));
    assert!(Arc::ptr_eq(&context.token_cache, &clone.token_cache));
    let (_, prepared) = prepare_main(
        &wrapper,
        Arc::new(Projection(4096)),
        &session,
        ModelRequest {
            model: "test".into(),
            messages: vec![message.clone()],
            ..Default::default()
        },
    )
    .await?;
    assert!(prepared.notice.is_none());
    assert_eq!(context.meters.lock().unwrap()[&session].pending, 4096);
    wrapper.observe_response(
        &session,
        &ModelResponse {
            last_request_usage: Some(bubblepaw_core::LastRequestUsage {
                input_tokens: Some(8192),
                output_tokens: Some(1),
            }),
            ..Default::default()
        },
    );
    assert_eq!(
        clone.meters.lock().unwrap()[&session].anchor,
        Some((4096, 8192))
    );
    context.message_tokens(&message);
    let after = context.token_cache.lock().unwrap().computations;
    clone.message_tokens(&message);
    assert_eq!(clone.token_cache.lock().unwrap().computations, after);
    Ok(())
}
