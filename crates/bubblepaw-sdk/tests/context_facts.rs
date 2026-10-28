//! Context readers use only the supported SDK modules.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_sdk::harness::{CancellationToken, Harness, HarnessConfig, RunOptions};
use bubblepaw_sdk::providers::{ModelRequest, ModelResponse, Provider, TextCallback, Usage};
use bubblepaw_sdk::store::{
    ContextBlock, ContextBlockKind, ContextChapter, ContextEpisode, ContextEvent, ContextEventKind,
    ContextSubjectChapter, ContextTrigger, ContextView, HistoryRange, MessageTokenSource,
    MessageTokens, Store, SummaryKind, context_events, context_structure, context_summary,
    context_view, history_range, message_tokens,
};
use bubblepaw_sdk::tools::ToolRegistry;
use std::sync::Arc;

struct ReportedAnswer;

#[async_trait]
impl Provider for ReportedAnswer {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "A recorded answer.".into(),
            usage: Some(Usage {
                output_tokens: 42,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn context_view_and_token_reads_need_no_core_escape_hatch() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("SDK context facts")?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(ReportedAnswer),
        ToolRegistry::default(),
        HarnessConfig::default(),
    );
    harness
        .prompt(&session.id, "Remember the answer.", RunOptions::default())
        .await?;
    let messages = store.messages(&session.id)?;
    let first = messages.first().unwrap();
    let last = messages.last().unwrap();
    let frontier = store.message_seq(&session.id, &last.id)?;
    let before_read = store.storage_stats()?.transaction_count;
    let counts: Vec<MessageTokens> = message_tokens(&store, &session.id, 0..=frontier)?;
    assert_eq!(counts.len(), messages.len());
    assert_eq!(
        counts.first().unwrap().source,
        MessageTokenSource::Estimated
    );
    let answer = counts.last().unwrap();
    assert_eq!(answer.message_id, last.id);
    assert_eq!(answer.source, MessageTokenSource::Reported);
    assert_eq!(answer.tokens, 42);
    assert!(counts.first().unwrap().tokens > 0);
    assert_eq!(store.storage_stats()?.transaction_count, before_read);
    assert_eq!(
        message_tokens(&store, &session.id, frontier..=frontier)?,
        vec![answer.clone()]
    );
    assert!(message_tokens(&store, &session.id, (frontier + 1)..=i64::MAX)?.is_empty());

    // Seed an already-summarized range through the existing chapter API. The
    // runner's scripted tests exercise automatic construction of these facts.
    store.save_context_chapter(&ContextChapter {
        id: "sdk-chapter".into(),
        session_id: session.id.clone(),
        ordinal: 1,
        first_message_id: first.id.clone(),
        last_message_id: last.id.clone(),
        title: "Remembered answer".into(),
        summary: "The answer was recorded.".into(),
        model: "scripted".into(),
        usage: None,
        created_at: last.created_at.clone(),
    })?;
    let view = ContextView {
        session_id: session.id.clone(),
        frontier: Some(frontier),
        system_prompt: String::new(),
        prefix: vec![first.clone()],
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        created_at: last.created_at.clone(),
        threshold: 100,
        budget: 80,
        tokens: 20,
        blocks: vec![ContextBlock {
            kind: ContextBlockKind::Opening,
            tokens: 20,
            id: None,
            first_message_id: Some(first.id.clone()),
            last_message_id: Some(first.id.clone()),
        }],
        blocks_estimated: true,
    };
    let event = ContextEvent {
        id: "sdk-apply".into(),
        session_id: session.id.clone(),
        kind: ContextEventKind::Applied,
        created_at: last.created_at.clone(),
        trigger: ContextTrigger::Manual,
        trigger_text: "user asked to compact".into(),
        first_message_id: first.id.clone(),
        last_message_id: last.id.clone(),
        episode_ids: Vec::new(),
        summary_id: Some("sdk-chapter".into()),
        tokens_before: Some(60),
        tokens_after: 6,
        input_before: Some(68),
        input_after: Some(20),
        input_estimated: true,
    };
    store.save_context_view_with_event(&view, &event)?;
    let before_reads = store.storage_stats()?.transaction_count;
    let events: Vec<ContextEvent> = context_events(&store, &session.id)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, ContextEventKind::Applied);
    let current: ContextView = context_view(&store, &session.id)?.unwrap();
    assert!(current.blocks_estimated);
    assert_eq!(current.blocks[0].kind, ContextBlockKind::Opening);
    assert_eq!(current.blocks[0].tokens, 20);
    let episodes: Vec<ContextEpisode> = store.context_episodes(&session.id)?;
    let chapters: Vec<ContextSubjectChapter> = store.context_subject_chapters(&session.id)?;
    assert!(episodes.is_empty());
    assert!(chapters.is_empty());
    let structure = context_structure(&store, &session.id)?;
    assert!(structure.episodes.is_empty());
    assert!(structure.chapters.is_empty());
    assert!(structure.summary_tokens_estimated);
    assert_eq!(structure.legacy_chapters.len(), 1);
    assert_eq!(structure.legacy_chapters[0].id, "sdk-chapter");
    assert_eq!(structure.legacy_chapters[0].status, "closed");
    assert_eq!(structure.legacy_chapters[0].source_tokens, None);
    assert!(structure.legacy_chapters[0].summary_tokens > 0);
    let summary = context_summary(&store, &session.id, SummaryKind::Chapter, "sdk-chapter")?;
    assert_eq!(summary.summary.as_deref(), Some("The answer was recorded."));
    let page = history_range(
        &store,
        &session.id,
        &HistoryRange {
            first_message_id: Some(first.id.clone()),
            last_message_id: Some(last.id.clone()),
            limit: Some(1),
            ..Default::default()
        },
    )?;
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].message.id, first.id);
    assert_eq!(page.messages[0].source, MessageTokenSource::Estimated);
    assert_eq!(page.next_seq, Some(frontier));
    let answer_page = history_range(
        &store,
        &session.id,
        &HistoryRange {
            first_seq: page.next_seq,
            last_seq: Some(frontier),
            ..Default::default()
        },
    )?;
    assert_eq!(answer_page.messages.len(), 1);
    assert_eq!(answer_page.messages[0].tokens, 42);
    assert_eq!(answer_page.messages[0].source, MessageTokenSource::Reported);
    assert_eq!(answer_page.next_seq, None);
    assert_eq!(store.storage_stats()?.transaction_count, before_reads);
    Ok(())
}
