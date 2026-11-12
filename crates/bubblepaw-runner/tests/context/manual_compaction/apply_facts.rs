use super::*;
use bubblepaw_core::{
    ContextBlockKind, ContextEventKind, ContextState, ContextTrigger, PrepareMode,
};
use bubblepaw_runner::summarization::{LcmConfig, canonical_message_tokens};
use serde_json::{Value, json};

/// A provider estimate with genuine extra wire framing. This exercises the max
/// of canonical and projected estimates without altering individual block costs.
struct Framed;
#[async_trait]
impl Provider for Framed {
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        tokenize: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        let canonical = tokenize(&json!(request.system_prompt))
            + tokenize(&json!(request.tools))
            + request
                .messages
                .iter()
                .map(canonical_message_tokens)
                .sum::<usize>();
        Ok(Some(canonical + 777))
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        bail!("apply must not dispatch a main-model request")
    }
}

fn assert_blocks(view: &bubblepaw_core::ContextView, request: &ModelRequest) {
    assert!(view.blocks_estimated);
    assert_eq!(view.blocks[0].kind, ContextBlockKind::Opening);
    assert_eq!(
        view.blocks.last().unwrap().kind,
        ContextBlockKind::RawSuffix
    );
    assert_eq!(
        view.blocks.iter().map(|block| block.tokens).sum::<u64>() + 8,
        view.tokens as u64
    );
    let raw = &request.messages[view.prefix.len()..];
    let suffix = view.blocks.last().unwrap();
    assert_eq!(
        suffix.first_message_id.as_deref(),
        raw.first().map(|message| message.id.as_str())
    );
    assert_eq!(
        suffix.last_message_id.as_deref(),
        raw.last().map(|message| message.id.as_str())
    );
    assert_eq!(
        suffix.tokens,
        raw.iter()
            .map(|message| canonical_message_tokens(message) as u64)
            .sum::<u64>()
    );
}

#[tokio::test]
async fn request_and_explicit_pressure_publish_facts_once_with_exact_input_estimates() -> Result<()>
{
    for (explicit, byte_only) in [(false, false), (true, false), (false, true)] {
        for projected in [false, true] {
            let f = Fixture::new(ContextStrategy::Lcm)?;
            let mut history = vec![
                f.message(Role::User, "Private initial task"),
                f.message(Role::Assistant, " datum".repeat(34_000)),
                f.message(Role::User, "Private latest task"),
                f.message(Role::Assistant, "Small current response"),
            ];
            if projected {
                history[1].provider_state = Some(json!({"adapter":"chat-completions",
                    "message_fields":{"reasoning_content":" retained thought".repeat(100)}}));
            }
            f.store.append_batch(&history)?;
            let provider: Arc<dyn Provider> = if projected {
                Arc::new(Framed)
            } else {
                Arc::new(NoMain)
            };
            let context = Arc::new(SummarizingContext::new(
                f.store.clone(),
                provider.clone(),
                SummaryConfig {
                    threshold_cap: if byte_only { 100_000 } else { 5_000 },
                    max_request_bytes: if byte_only { 20_000 } else { 2 * 1024 * 1024 },
                    lcm: LcmConfig {
                        output_reserve_tokens: 0,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                None,
            )?);
            let worker = new_episode_worker(f.store.clone(), f.models.clone(), context.clone());
            context.set_episode_worker(&worker);
            let request = ModelRequest {
                model: "scripted".into(),
                system_prompt: "Keep original instructions".into(),
                messages: history.clone(),
                ..Default::default()
            };
            let input_before = context.estimated_tokens(&request);
            // Gathering produces durable memory without publishing any view.
            worker.precompute(&f.session).await?;
            assert!(!f.store.context_episodes(&f.session)?.is_empty());
            assert!(f.store.context_view(&f.session)?.is_none());
            assert!(
                f.store
                    .context_events(&f.session)?
                    .iter()
                    .all(|event| event.kind != ContextEventKind::Applied)
            );
            f.store.flush_serving()?;
            let before = f.store.storage_stats()?;
            let mode = if explicit {
                PrepareMode::Pressure {
                    additional_tokens: 100,
                    additional_bytes: 0,
                }
            } else {
                PrepareMode::Request
            };
            let prepared = prepare_call(
                &*context,
                provider.clone(),
                &ContextState::default(),
                &f.session,
                request.clone(),
                mode,
                CancellationToken::new(),
            )
            .await?;
            let view = f.store.context_view(&f.session)?.unwrap();
            assert_eq!(f.store.messages(&f.session)?, history);
            assert_eq!(prepared.request.system_prompt, request.system_prompt);
            assert_eq!(
                serde_json::to_value(&prepared.request.tools)?,
                serde_json::to_value(&request.tools)?
            );
            let events = f.store.context_events(&f.session)?;
            let applied = events
                .iter()
                .filter(|event| event.kind == ContextEventKind::Applied)
                .collect::<Vec<_>>();
            assert_eq!(applied.len(), 1);
            let event = applied[0];
            assert_eq!(event.trigger, ContextTrigger::Pressure);
            assert_eq!(event.first_message_id, history[0].id);
            assert_eq!(event.last_message_id, history[1].id);
            assert_eq!(event.input_before, Some(input_before as u64));
            assert_eq!(
                event.input_after,
                Some(context.estimated_tokens(&prepared.request) as u64)
            );
            assert!(event.input_estimated);
            if byte_only {
                assert!(event.input_before.unwrap() < 100_000);
                assert!(event.trigger_text.contains("bytes, cap 20000"));
            } else {
                assert!(event.input_before.unwrap() > 5_000);
            }
            assert!(event.input_before > event.input_after);
            assert!(event.input_after.unwrap() <= if explicit { 4_900 } else { 5_000 });
            let canonical_source = history[..2]
                .iter()
                .map(|message| canonical_message_tokens(message) as u64)
                .sum::<u64>();
            let episodes = f.store.context_episodes(&f.session)?;
            assert_eq!(event.tokens_before, Some(episodes[0].source_tokens as u64));
            if projected {
                assert!(event.tokens_before.unwrap() > canonical_source);
            } else {
                assert_eq!(event.tokens_before, Some(canonical_source));
            }
            assert!(event.tokens_before.unwrap() > event.tokens_after);
            assert!(event.trigger_text.chars().count() <= 160);
            assert!(event.trigger_text.starts_with("Input reached "));
            assert!(!event.trigger_text.contains("Private"));
            assert!(!event.trigger_text.contains("Original decisions"));
            assert_eq!(event.episode_ids, vec![episodes[0].id.clone()]);
            assert_eq!(event.summary_id.as_deref(), Some(episodes[0].id.as_str()));
            assert_blocks(&view, &prepared.request);
            if projected {
                let canonical_messages = prepared
                    .request
                    .messages
                    .iter()
                    .map(canonical_message_tokens)
                    .sum::<usize>();
                let tokenizer =
                    tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
                let header = tokenizer
                    .encode_ordinary(&prepared.request.system_prompt)
                    .len()
                    + tokenizer
                        .encode_ordinary(&json!(prepared.request.tools).to_string())
                        .len();
                let excess = view.tokens - canonical_messages - header - 8;
                assert!(excess >= 777);
                assert_eq!(view.blocks[0].tokens, (header + excess) as u64);
            }
            let summary = view
                .blocks
                .iter()
                .find(|block| block.kind == ContextBlockKind::EpisodeSummary)
                .unwrap();
            assert_eq!(summary.id.as_deref(), Some(episodes[0].id.as_str()));
            assert_eq!(
                summary.first_message_id.as_deref(),
                Some(history[0].id.as_str())
            );
            assert_eq!(
                summary.last_message_id.as_deref(),
                Some(history[1].id.as_str())
            );
            // The apply fact shares the existing chat validation commit. The
            // serving view is immediately readable in its buffered transaction
            // and commits only when the host flushes that projection.
            let pending = f.store.storage_stats()?;
            assert_eq!(
                pending.chat_transaction_count - before.chat_transaction_count,
                1
            );
            assert_eq!(
                pending.serving_transaction_count,
                before.serving_transaction_count
            );
            assert_eq!(pending.transaction_count - before.transaction_count, 1);
            f.store.flush_serving()?;
            let after = f.store.storage_stats()?;
            assert_eq!(
                after.chat_transaction_count - before.chat_transaction_count,
                1
            );
            assert_eq!(
                after.serving_transaction_count - before.serving_transaction_count,
                1
            );
            assert_eq!(after.transaction_count - before.transaction_count, 2);
            let frozen = serde_json::to_value(&view)?;
            let reused = prepare_fresh(&*context, provider, &f.session, request).await?;
            assert_eq!(
                serde_json::to_value(&reused.request)?,
                serde_json::to_value(&prepared.request)?
            );
            f.store.flush_serving()?;
            let reused_stats = f.store.storage_stats()?;
            assert_eq!(
                reused_stats.chat_transaction_count,
                after.chat_transaction_count
            );
            assert_eq!(
                reused_stats.serving_transaction_count,
                after.serving_transaction_count
            );
            assert_eq!(reused_stats.transaction_count, after.transaction_count);
            assert_eq!(
                serde_json::to_value(f.store.context_view(&f.session)?.unwrap())?,
                frozen
            );
            assert_eq!(
                f.store
                    .context_events(&f.session)?
                    .iter()
                    .filter(|event| event.kind == ContextEventKind::Applied)
                    .count(),
                1
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn manual_apply_records_manual_trigger_and_noop_keeps_event_count() -> Result<()> {
    let f = Fixture::new(ContextStrategy::Lcm)?;
    f.turns(3)?;
    let history = f.store.messages(&f.session)?;
    f.harness
        .compact_session(&f.session, Some("PRIVATE FOCUS MUST NOT LEAK"))
        .await?;
    let events = f.store.context_events(&f.session)?;
    let applied = events
        .iter()
        .filter(|event| event.kind == ContextEventKind::Applied)
        .collect::<Vec<_>>();
    assert_eq!(applied.len(), 1);
    let event = applied[0];
    assert_eq!(event.trigger, ContextTrigger::Manual);
    assert_eq!(event.trigger_text, "User asked to compact");
    assert_eq!(event.first_message_id, history[0].id);
    assert_eq!(event.last_message_id, history[3].id);
    assert!(event.input_estimated);
    assert!(event.input_before.is_some());
    assert!(event.input_after.is_some());
    let view = f.store.context_view(&f.session)?.unwrap();
    assert!(view.blocks_estimated);
    assert_eq!(
        view.blocks.iter().map(|block| block.tokens).sum::<u64>() + 8,
        view.tokens as u64
    );
    let tx = f.store.storage_stats()?.transaction_count;
    f.harness.compact_session(&f.session, None).await?;
    assert_eq!(f.store.context_events(&f.session)?, events);
    assert_eq!(f.store.storage_stats()?.transaction_count, tx);
    Ok(())
}

#[tokio::test]
async fn calibrated_pressure_labels_its_measurement_without_changing_raw_input_fact() -> Result<()>
{
    let f = Fixture::new(ContextStrategy::Lcm)?;
    f.turns(3)?;
    let context = Arc::new(SummarizingContext::new(
        f.store.clone(),
        Arc::new(NoMain),
        SummaryConfig {
            threshold_cap: 5_000,
            lcm: LcmConfig {
                output_reserve_tokens: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    )?);
    let worker = new_episode_worker(f.store.clone(), f.models.clone(), context.clone());
    context.set_episode_worker(&worker);
    let request = ModelRequest {
        messages: f.store.messages(&f.session)?,
        ..Default::default()
    };
    let raw = context.estimated_tokens(&request);
    assert!(raw < 5_000);
    prepare_fresh(&*context, Arc::new(NoMain), &f.session, request.clone()).await?;
    bubblepaw_core::ContextPreparation::observe_response(
        &*context,
        &f.session,
        &ModelResponse {
            usage: Some(Usage {
                input_tokens: 7_000,
                output_tokens: 0,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    prepare_fresh(&*context, Arc::new(NoMain), &f.session, request).await?;
    let events = f.store.context_events(&f.session)?;
    let applied = events
        .iter()
        .find(|event| event.kind == ContextEventKind::Applied)
        .unwrap();
    assert_eq!(applied.input_before, Some(raw as u64));
    assert!(
        applied
            .trigger_text
            .starts_with("Calibrated input reached ")
    );
    assert!(applied.trigger_text.ends_with("tokens, cap 5000"));
    assert_eq!(applied.trigger, ContextTrigger::Pressure);
    Ok(())
}

#[path = "apply_facts_imports.rs"]
mod imported;
