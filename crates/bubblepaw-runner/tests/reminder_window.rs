//! Final reminder admission uses captured-provider sizing without preparing again.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Capabilities, ContextPreparation, ContextState, ModelRequest, ModelResponse,
    Provider, Reminder, Store, TextCallback, Usage,
};
use bubblepaw_runner::{
    summarization::{LcmConfig, SummarizingContext, SummaryConfig},
    usage::UsageContext,
};
use serde_json::Value;
use std::sync::Arc;

#[path = "common/hook.rs"]
mod hook;
use hook::prepare_fresh;

struct Estimate {
    tokens: Option<usize>,
    window: usize,
}
#[async_trait]
impl Provider for Estimate {
    fn capabilities(&self, _: &str) -> Capabilities {
        Capabilities {
            context_window: Some(self.window),
            ..Default::default()
        }
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        _: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Ok(Some(self.tokens.unwrap_or_else(|| {
            request.system_prompt.len()
                + request
                    .messages
                    .iter()
                    .map(|message| message.content.len())
                    .sum::<usize>()
        })))
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("pure final validation must never dispatch")
    }
}
fn context(store: &Store, provider: Arc<dyn Provider>) -> Result<SummarizingContext> {
    SummarizingContext::new(
        store.clone(),
        provider,
        SummaryConfig {
            context_window: Some(500),
            threshold_cap: 500,
            lcm: LcmConfig {
                output_reserve_tokens: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    )
}

#[test]
fn final_validation_uses_captured_provider_and_reserves_output_exactly_once() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("captured window")?.id;
    let fallback = Arc::new(Estimate {
        tokens: Some(1),
        window: 500,
    });
    let captured = Arc::new(Estimate {
        tokens: Some(80),
        window: 100,
    });
    let context = context(&store, fallback)?;
    let mut request = ModelRequest {
        model: "fixture/model".into(),
        max_output_tokens: Some(20),
        ..Default::default()
    };
    context.validate_prepared_request(&session, &request, captured.clone())?;
    request.max_output_tokens = Some(21);
    assert!(
        context
            .validate_prepared_request(&session, &request, captured)
            .is_err()
    );
    Ok(())
}

#[test]
fn oversized_final_reminder_keeps_queue_and_durable_history_unchanged() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("tail guard")?.id;
    let provider = Arc::new(Estimate {
        tokens: None,
        window: 100,
    });
    let context = Arc::new(UsageContext::new(
        store.clone(),
        Arc::new(context(&store, provider.clone())?),
    ));
    let state = ContextState::default();
    state.remind_with_metadata(
        &session,
        "root",
        Reminder::new("fixture", "oversized ".repeat(100)),
    );
    let before = store.storage_stats()?.chat_transaction_count;
    let result = state.build_persisted(
        ModelRequest::default(),
        &session,
        "root",
        &[],
        &store,
        |request| context.validate_prepared_request(&session, request, provider.clone()),
    );
    assert!(result.is_err());
    assert!(store.messages(&session)?.is_empty());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    state.remind_with_metadata(&session, "root", Reminder::new("fixture", "ok"));
    let request = state
        .build_persisted(
            ModelRequest::default(),
            &session,
            "root",
            &[],
            &store,
            |request| context.validate_prepared_request(&session, request, provider.clone()),
        )?
        .request;
    assert!(
        request
            .messages
            .last()
            .unwrap()
            .content
            .contains(">ok</harness>")
    );
    assert_eq!(store.messages(&session)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn final_guard_retains_calibrated_input_anchor_without_advancing_it() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("calibrated guard")?.id;
    let provider = Arc::new(Estimate {
        tokens: Some(20),
        window: 100,
    });
    let context = context(&store, provider.clone())?;
    let request = ModelRequest {
        model: "fixture/model".into(),
        ..Default::default()
    };
    prepare_fresh(&context, provider.clone(), &session, request.clone()).await?;
    context.observe_response(
        &session,
        &ModelResponse {
            usage: Some(Usage {
                input_tokens: 120,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let before = store.storage_stats()?.chat_transaction_count;
    assert!(
        context
            .validate_prepared_request(&session, &request, provider.clone())
            .is_err()
    );
    assert!(
        context
            .validate_prepared_request(&session, &request, provider)
            .is_err()
    );
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    assert!(store.context_view(&session)?.is_none());
    Ok(())
}

#[tokio::test]
async fn response_calibration_uses_the_dispatched_reminder_instead_of_counting_it_twice()
-> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("final input calibration")?.id;
    let provider = Arc::new(Estimate {
        tokens: None,
        window: 100,
    });
    let context = context(&store, provider.clone())?;
    let request = ModelRequest {
        model: "fixture/model".into(),
        ..Default::default()
    };
    prepare_fresh(&context, provider.clone(), &session, request.clone()).await?;
    let mut dispatched = request;
    dispatched.messages.push(bubblepaw_core::Message {
        id: "final-reminder".into(),
        session_id: session.clone(),
        role: bubblepaw_core::Role::User,
        content: "r".repeat(80),
        origin: Some(bubblepaw_core::MessageOrigin::Generated),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-23T00:00:00Z".into(),
    });
    context.validate_prepared_request(&session, &dispatched, provider.clone())?;
    context.observe_request(&session, &dispatched, provider.clone());
    context.observe_response(
        &session,
        &ModelResponse {
            usage: Some(Usage {
                input_tokens: context.estimated_tokens(&dispatched) as u64,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let prepared = prepare_fresh(&context, provider.clone(), &session, dispatched).await?;
    context.validate_prepared_request(&session, &prepared.request, provider)?;
    assert!(
        store.context_view(&session)?.is_none(),
        "accounting alone needs no apply"
    );
    Ok(())
}

#[test]
fn disabled_automatic_summary_admission_keeps_existing_bypass_with_or_without_view() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("ordinary no-auto history")?.id;
    let provider = Arc::new(Estimate {
        tokens: Some(10_000),
        window: 1,
    });
    let context = context(&store, provider.clone())?.with_auto_summarize(false);
    let request = ModelRequest::default();
    context.validate_prepared_request(&session, &request, provider.clone())?;
    store.save_context_view(&bubblepaw_core::ContextView {
        session_id: session.clone(),
        frontier: None,
        system_prompt: String::new(),
        prefix: vec![],
        masked_tool_output_ids: vec![],
        clipped_tool_call_ids: vec![],
        created_at: "2026-10-21T00:00:00Z".into(),
        threshold: 20,
        budget: 20,
        tokens: 0,
    })?;
    context.validate_prepared_request(&session, &request, provider)?;
    Ok(())
}
