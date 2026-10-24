mod support;

use anyhow::Result;
use bubblepaw_core::{ContextCheckpoint, ContextStrategy, ContextView, Role, Store};
use rusqlite::{Connection, params};
use serde_json::json;
use support::stored_message;
const DATE: &str = "2026-10-12T00:00:00Z";
#[test]
fn large_content_is_deduplicated_and_every_history_read_reconstructs_it() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("chat.db");
    let store = Store::open(&path)?;
    let session = store.create_session("blobs")?;
    let mut content = format!(
        "<BubblePawAttachments>{}</BubblePawAttachments>",
        "猫\n".repeat(3000)
    );
    content.push_str(&"x".repeat(100 * 1024 - content.len()));
    assert_eq!(content.len(), 100 * 1024);
    let mut tool = stored_message(&session.id, "tool", Role::Tool, &content);
    tool.tool_call_id = Some("call".into());
    tool.provider_state = Some(json!({"signature":"opaque", "thinking":"preserved"}));
    let mut duplicate = tool.clone();
    duplicate.id = "duplicate-tool".into();
    duplicate.tool_call_id = Some("duplicate-call".into());
    let attachment = stored_message(&session.id, "attachment", Role::User, &content);
    let boundary = stored_message(&session.id, "boundary", Role::Tool, &"x".repeat(4096));
    let before = store.storage_stats()?.chat_transaction_count;
    store.append_batch(&[
        tool.clone(),
        duplicate.clone(),
        attachment.clone(),
        boundary.clone(),
    ])?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);
    store.append_batch(&[])?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);

    let conn = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    let (count, raw, compressed): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*),SUM(raw_bytes),SUM(length(data)) FROM blobs",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(count, 1);
    assert_eq!(raw, content.len() as i64);
    assert!(compressed < raw / 4);
    let stored: String = conn.query_row(
        "SELECT data FROM messages WHERE message_id='tool'",
        [],
        |row| row.get(0),
    )?;
    assert!(!stored.contains(&content));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored)?["content"],
        ""
    );
    let boundary_json: String = conn.query_row(
        "SELECT data FROM messages WHERE message_id='boundary'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&boundary_json)?["content"],
        boundary.content
    );

    let expected = serde_json::to_value([tool, duplicate, attachment, boundary])?;
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        expected
    );
    assert_eq!(
        serde_json::to_value(store.snapshot(&session.id, 10, 10, 10)?.messages)?,
        expected
    );
    let records = store.messages_after(&session.id, 0)?;
    assert_eq!(
        serde_json::to_value(
            records
                .iter()
                .map(|(_, message)| message)
                .collect::<Vec<_>>()
        )?,
        expected
    );
    assert_eq!(store.messages_after(&session.id, records[2].0)?.len(), 1);
    assert_eq!(
        store.session_summaries(10)?[0]
            .last_message
            .as_ref()
            .unwrap()
            .content,
        "x".repeat(4096)
    );
    let fork = store.fork(&session.id, "fork")?;
    assert_eq!(store.messages(&fork.id)?[0].content, content);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))?,
        1
    );
    drop(conn);
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(
        serde_json::to_value(reopened.messages(&session.id)?)?,
        expected
    );
    reopened.idle_checkpoint()?;
    assert_eq!(reopened.storage_stats()?.wal_bytes, 0);
    Ok(())
}

#[test]
fn view_uses_ranges_and_immutable_summary_refs_with_exact_roundtrip() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("view.db");
    let store = Store::open(&path)?;
    let session = store.create_session_with_strategy("view", ContextStrategy::Classical)?;
    let first = stored_message(
        &session.id,
        "first",
        Role::User,
        "canonical user words with Unicode 猫 to preserve exactly",
    );
    let second = stored_message(&session.id, "second", Role::User, "new request");
    store.append_batch(&[first.clone(), second.clone()])?;
    let checkpoint = ContextCheckpoint {
        id: "summary-old".into(),
        session_id: session.id.clone(),
        through_message_id: first.id.clone(),
        model: "scripted".into(),
        summary: "unique old summary text with 猫 and line\nbreak".into(),
        files_touched: Vec::new(),
        created_at: DATE.into(),
        usage: None,
    };
    store.save_context_checkpoint(&checkpoint)?;
    let view = ContextView {
        session_id: session.id.clone(),
        frontier: Some(store.message_seq(&session.id, &first.id)?),
        system_prompt: "frozen instructions".into(),
        prefix: vec![
            first.clone(),
            second,
            stored_message(
                &session.id,
                "synthetic",
                Role::System,
                &format!(
                    "Summary:\n{}\nUser excerpt:\n{}",
                    checkpoint.summary,
                    &first.content[10..]
                ),
            ),
        ],
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        created_at: DATE.into(),
        threshold: 200,
        budget: 100,
        tokens: 42,
    };
    // Saving a captured view after the worker replaces its latest checkpoint must retain C1.
    store.save_context_checkpoint(&ContextCheckpoint {
        id: "summary-new".into(),
        through_message_id: "second".into(),
        summary: "new summary".into(),
        ..checkpoint.clone()
    })?;
    store.save_context_view(&view)?;
    store.flush_serving()?;
    let conn = Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    let data: String = conn.query_row("SELECT data FROM context_views", [], |row| row.get(0))?;
    let pointers: serde_json::Value = serde_json::from_str(&data)?;
    assert!(pointers.get("prefix").is_none());
    assert_eq!(pointers["prefix_refs"][0]["kind"], "messages");
    assert!(!data.contains(&checkpoint.summary));
    assert!(!data.contains(&first.content));
    assert!(data.contains("summary-old"));
    assert_eq!(
        serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
        serde_json::to_value(&view)?
    );
    assert_eq!(
        store.context_checkpoint(&session.id)?.unwrap().summary,
        "new summary"
    );
    drop(conn);
    drop(store);
    assert_eq!(
        serde_json::to_value(Store::open(&path)?.context_view(&session.id)?.unwrap())?,
        serde_json::to_value(view)?
    );
    Ok(())
}

#[test]
fn old_inline_rows_are_not_rewritten_when_message_ids_are_backfilled() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("old.db");
    let conn = Connection::open(&path)?;
    conn.execute_batch(include_str!("fixtures/main-v6-schema.sql"))?;
    let session = json!({"id":"old", "title":"old", "parent_session_id":null,"parent_cutoff":null,
        "created_at":DATE,"updated_at":DATE,"read_only":false});
    conn.execute(
        "INSERT INTO sessions VALUES('old',?1)",
        [session.to_string()],
    )?;
    let original = serde_json::to_string_pretty(&stored_message(
        "old",
        "old-tool",
        Role::Tool,
        &"legacy inline\n".repeat(1000),
    ))?;
    conn.execute(
        "INSERT INTO messages(session_id,data) VALUES('old',?1)",
        [&original],
    )?;
    drop(conn);
    let store = Store::open(&path)?;
    assert_eq!(store.message_seq("old", "old-tool")?, 1);
    assert_eq!(
        store.messages("old")?[0].content,
        "legacy inline\n".repeat(1000)
    );
    let conn = Connection::open(store.chat_path("old")?.unwrap())?;
    assert_eq!(
        conn.query_row("SELECT data FROM messages", [], |row| row
            .get::<_, String>(0))?,
        original
    );
    assert_eq!(
        conn.query_row("SELECT message_id FROM messages", [], |row| row
            .get::<_, String>(0))?,
        "old-tool"
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        11
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id=?1 AND message_id=?2",
            params!["old", "old-tool"],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[test]
fn projected_canonical_prefix_preserves_clipped_arguments_and_provider_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("projected.db");
    let store = Store::open(&path)?;
    let session = store.create_session("projected")?;
    let content = "Original full output retained canonically. ".repeat(100);
    let arguments = "a substantial original argument string with unique words ".repeat(100);
    let mut original = stored_message(&session.id, "projected-call", Role::Assistant, &content);
    original.tool_calls = vec![bubblepaw_core::ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments: json!({"input":arguments}),
    }];
    original.provider_state = Some(json!({"signature":"provider-owned byte-exact signature"}));
    store.append_batch(&[original.clone()])?;
    let mut projected = original.clone();
    projected.content = content[..100].into();
    projected.tool_calls[0].arguments = json!({"clipped":true,"preview":&arguments[..200]});
    let view = ContextView {
        session_id: session.id.clone(),
        frontier: None,
        system_prompt: String::new(),
        prefix: vec![projected],
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        created_at: DATE.into(),
        threshold: 200,
        budget: 100,
        tokens: 42,
    };
    store.save_context_view(&view)?;
    assert_eq!(
        serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
        serde_json::to_value(&view)?
    );
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?[0].clone())?,
        serde_json::to_value(original)?
    );
    store.flush_serving()?;
    let conn = Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    let data: String = conn.query_row("SELECT data FROM context_views", [], |row| row.get(0))?;
    assert!(!data.contains(&content[..100]));
    assert!(!data.contains(&arguments[..200]));
    assert!(data.contains("json:tool_calls"));
    drop(conn);
    drop(store);
    assert_eq!(
        serde_json::to_value(Store::open(&path)?.context_view(&session.id)?.unwrap())?,
        serde_json::to_value(view)?
    );
    Ok(())
}

#[test]
fn frozen_episode_view_survives_open_chapter_growth_and_closure() -> Result<()> {
    use bubblepaw_core::{
        ContextEpisode, ContextSubjectChapter, EpisodeBatch, SubjectChapterStatus,
    };
    let store = Store::in_memory()?;
    let session = store.create_session("episodes")?;
    let first = stored_message(&session.id, "episode-source-1", Role::User, "first request");
    let second = stored_message(
        &session.id,
        "episode-source-2",
        Role::User,
        "second request",
    );
    store.append_batch(&[first.clone(), second.clone()])?;
    let chapter = ContextSubjectChapter {
        id: "chapter-1".into(),
        session_id: session.id.clone(),
        ordinal: 1,
        first_episode_ordinal: 1,
        last_episode_ordinal: 1,
        status: SubjectChapterStatus::Open,
        description: None,
        summary: None,
        related_ids: Vec::new(),
        related_evidence: Vec::new(),
        model: None,
        usage: None,
        reasoning: None,
    };
    let episode = ContextEpisode {
        id: "episode-1".into(),
        session_id: session.id.clone(),
        ordinal: 1,
        first_message_id: first.id.clone(),
        last_message_id: first.id.clone(),
        source_tokens: 100,
        cap: 50,
        origin_request_id: None,
        title: "First episode title".into(),
        summary: "First episode immutable summary body".into(),
        model: "scripted".into(),
        usage: None,
        reasoning: None,
        created_at: DATE.into(),
        chapter_id: chapter.id.clone(),
    };
    store.save_episode_batch(&EpisodeBatch {
        episode: episode.clone(),
        decisions: Vec::new(),
        open_chapter: chapter.clone(),
        closed_chapter: None,
        close_after_episode: None,
        expected_previous_coverage: None,
    })?;
    let view = ContextView {
        session_id: session.id.clone(),
        frontier: Some(store.message_seq(&session.id, &first.id)?),
        system_prompt: String::new(),
        prefix: vec![stored_message(
            &session.id,
            "episode-render",
            Role::System,
            &format!(
                "Open chapter episodes:\n{}\n{}",
                episode.title, episode.summary
            ),
        )],
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        created_at: DATE.into(),
        threshold: 200,
        budget: 100,
        tokens: 42,
    };
    store.save_context_view(&view)?;
    let grown = ContextSubjectChapter {
        last_episode_ordinal: 2,
        ..chapter
    };
    let closed = ContextSubjectChapter {
        status: SubjectChapterStatus::Closed,
        description: Some("A closed subject".into()),
        summary: Some("Fresh chapter summary replaces the open block for future views".into()),
        model: Some("scripted".into()),
        ..grown.clone()
    };
    store.save_episode_batch(&EpisodeBatch {
        episode: ContextEpisode {
            id: "episode-2".into(),
            ordinal: 2,
            first_message_id: second.id.clone(),
            last_message_id: second.id,
            title: "Second episode".into(),
            summary: "Second episode summary".into(),
            ..episode
        },
        decisions: Vec::new(),
        open_chapter: grown,
        closed_chapter: None,
        close_after_episode: Some(closed),
        expected_previous_coverage: Some(first.id),
    })?;
    assert_eq!(
        serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
        serde_json::to_value(view)?
    );
    assert_eq!(
        store.context_subject_chapters(&session.id)?[0].status,
        SubjectChapterStatus::Closed
    );
    Ok(())
}

#[test]
fn same_id_view_projection_preserves_signed_zero_arguments_and_provider_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("signed-zero-view.db");
    let store = Store::open(&path)?;
    let session = store.create_session("signed-zero view")?;
    let mut original = stored_message(&session.id, "same-id", Role::Assistant, "unchanged content");
    original.tool_calls = vec![bubblepaw_core::ToolCall {
        id: "signed-zero-call".into(),
        name: "tool".into(),
        arguments: json!({"number":0.0}),
    }];
    original.provider_state = Some(json!({"number":0.0,"signature":"opaque metadata"}));
    store.append_batch(std::slice::from_ref(&original))?;
    let mut projected = original.clone();
    projected.tool_calls[0].arguments = json!({"number":-0.0});
    projected.provider_state = Some(json!({"number":-0.0,"signature":"opaque metadata"}));
    assert_ne!(
        serde_json::to_vec(&original)?,
        serde_json::to_vec(&projected)?
    );
    let view = ContextView {
        session_id: session.id.clone(),
        frontier: None,
        system_prompt: String::new(),
        prefix: vec![projected],
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        created_at: DATE.into(),
        threshold: 200,
        budget: 100,
        tokens: 42,
    };
    let expected = serde_json::to_vec(&view)?;
    store.save_context_view(&view)?;
    assert_eq!(
        serde_json::to_vec(&store.context_view(&session.id)?.unwrap())?,
        expected
    );
    assert_eq!(
        serde_json::to_vec(&store.messages(&session.id)?[0])?,
        serde_json::to_vec(&original)?
    );
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(
        serde_json::to_vec(&reopened.context_view(&session.id)?.unwrap())?,
        expected
    );
    Ok(())
}

#[tokio::test]
async fn attachment_bytes_and_inline_json_use_one_blob_with_exact_escaped_roundtrip() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("attachments.db");
    let store = Store::open(&path)?;
    let session = store.create_session("attachments")?;
    let text = "Text with 猫, quotes \"like this\", a slash \\ and\nnewlines.\nLiteral tags: <BubblePawAttachments> and </BubblePawAttachments>.\n".repeat(500);
    let metadata = "{\"id\":\"file-1\",\"name\":\"notes.txt\",\"text\":true}";
    let hash = store.store_attachment(&session.id, "file-1", metadata, text.as_bytes())?;
    assert_eq!(
        store.store_attachment(&session.id, "file-1", metadata, text.as_bytes())?,
        hash
    );
    assert!(
        store
            .store_attachment(&session.id, "file-1", metadata, b"replacement")
            .is_err()
    );
    assert!(
        store
            .store_attachment(&session.id, "file-1", "{}", text.as_bytes())
            .is_err()
    );
    let inline =
        json!({"files":[{"id":"file-1","name":"notes.txt","size":text.len(),"inline":text}]});
    let content =
        format!("Please inspect.\n\n<BubblePawAttachments>\n{inline}\n</BubblePawAttachments>");
    store.queue(&session.id, bubblepaw_core::InputKind::Followup, &content)?;
    assert_eq!(store.queued_inputs(&session.id)?[0].text, content);
    assert_eq!(
        store.snapshot(&session.id, 10, 10, 10)?.pending_inputs[0].text,
        content
    );
    let conn = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    let descriptor: String =
        conn.query_row("SELECT content_blob FROM inputs", [], |row| row.get(0))?;
    assert_eq!(
        conn.query_row("SELECT text FROM inputs", [], |row| row.get::<_, String>(0))?,
        ""
    );
    let agent = input_harness(&store);
    agent.resume(&session.id, Default::default()).await?;
    assert_eq!(
        store.attachment(&session.id, "file-1")?,
        Some((metadata.into(), text.as_bytes().to_vec()))
    );
    assert!(store.attachment(&session.id, "missing")?.is_none());
    assert_eq!(store.messages(&session.id)?[0].content, content);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs WHERE hash=?1", [&hash], |row| {
            row.get::<_, i64>(0)
        })?,
        1
    );
    assert_eq!(
        conn.query_row("SELECT blob_hash FROM attachments", [], |row| row
            .get::<_, String>(0))?,
        hash
    );
    let data: String = conn.query_row(
        "SELECT data FROM messages WHERE json_extract(data,'$.role')='user'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&data)?["_content_parts_blob"],
        descriptor
    );
    assert!(store.queued_inputs(&session.id)?[0].text.is_empty());
    assert!(!data.contains(&text));
    drop(conn);
    drop(agent);
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(reopened.messages(&session.id)?[0].content, content);
    assert_eq!(
        reopened.attachment(&session.id, "file-1")?,
        Some((metadata.into(), text.into_bytes()))
    );
    Ok(())
}

struct InputProvider;
#[async_trait::async_trait]
impl bubblepaw_core::Provider for InputProvider {
    async fn generate(
        &self,
        _: bubblepaw_core::ModelRequest,
        _: bubblepaw_core::TextCallback,
        _: bubblepaw_core::CancellationToken,
    ) -> Result<bubblepaw_core::ModelResponse> {
        Ok(bubblepaw_core::ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}

fn input_harness(store: &Store) -> bubblepaw_core::Harness {
    bubblepaw_core::Harness::new(
        store.clone(),
        std::sync::Arc::new(InputProvider),
        bubblepaw_core::ToolRegistry::new(),
        Default::default(),
    )
}

#[tokio::test]
async fn accepted_input_transfers_one_blob_from_pending_queue_to_canonical_message() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("accepted.db");
    let store = Store::open(&path)?;
    let session = store.create_session("accepted")?;
    let text = "Small accepted input containing Unicode 猫 and\nnewlines.";
    let input = store.queue(&session.id, bubblepaw_core::InputKind::Followup, text)?;
    let conn = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    let hash: String = conn.query_row(
        "SELECT content_blob FROM inputs WHERE id=?1",
        [&input.id],
        |row| row.get(0),
    )?;
    assert_eq!(
        conn.query_row("SELECT text FROM inputs WHERE id=?1", [&input.id], |row| {
            row.get::<_, String>(0)
        })?,
        ""
    );
    assert_eq!(store.queued_inputs(&session.id)?[0].text, text);
    assert_eq!(
        store.snapshot(&session.id, 10, 10, 10)?.pending_inputs[0].text,
        text
    );
    input_harness(&store)
        .resume(&session.id, Default::default())
        .await?;
    let data: String = conn.query_row(
        "SELECT data FROM messages WHERE json_extract(data,'$.role')='user'",
        [],
        |row| row.get(0),
    )?;
    let value: serde_json::Value = serde_json::from_str(&data)?;
    assert_eq!(value["content"], "");
    assert_eq!(value["_content_blob"], hash);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))?,
        1
    );
    store.flush_serving()?;
    let serving = Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    assert!(
        serving
            .query_row(
                "SELECT content_blob FROM inputs WHERE id=?1",
                [&input.id],
                |row| row.get::<_, Option<String>>(0)
            )?
            .is_none()
    );
    assert!(store.queued_inputs(&session.id)?[0].text.is_empty());
    assert_eq!(store.messages(&session.id)?[0].content, text);
    drop(conn);
    drop(store);
    assert_eq!(Store::open(&path)?.messages(&session.id)?[0].content, text);
    Ok(())
}

#[path = "store_storage/script_failures.rs"]
mod script_failures;
