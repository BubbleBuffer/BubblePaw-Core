//! Phase B acceptance: the disposable serving file is never the only copy of a record.
mod support;

use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, CancellationToken, ContextCheckpoint, ContextEpisode, ContextPreparation,
    ContextStrategy, ContextSubjectChapter, ContextView, EpisodeBatch, EventKind, Harness,
    HarnessConfig, InputKind, Message, ModelRequest, ModelResponse, PrepareCall, PreparedContext,
    Provider, Role, RunOptions, Session, Store, StoreOptions, SubjectChapterStatus, TextCallback,
    Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult, Usage,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use support::stored_message;
const DATE: &str = "2026-10-12T00:00:00Z";
fn chat_path(store: &Store, session: &str) -> Result<PathBuf> {
    Ok(store
        .chat_path(session)?
        .expect("persistent chat has a file"))
}

fn has_table(path: &Path, table: &str) -> Result<bool> {
    Ok(Connection::open(path)?.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn rows(path: &Path, table: &str) -> Result<i64> {
    Ok(
        Connection::open(path)?.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })?,
    )
}

/// Observe the complete prepared request before the provider's disposable cache
/// epoch is assigned. Its serialized bytes include every model-facing field.
#[derive(Default)]
struct Requests(Mutex<Vec<Vec<u8>>>);
#[async_trait]
impl ContextPreparation for Requests {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let request = call.request;
        self.0.lock().unwrap().push(serde_json::to_vec(&request)?);
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
}

#[derive(Default)]
struct Model {
    requests: Mutex<Vec<ModelRequest>>,
    stop_after_capture: bool,
}
#[async_trait]
impl Provider for Model {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request.clone());
        if self.stop_after_capture {
            bail!("capture complete")
        }
        let last = request
            .messages
            .last()
            .expect("scripted request has history");
        if last.role == Role::User {
            return Ok(ModelResponse {
                text: "calling a tool".into(),
                tool_calls: vec![ToolCall {
                    id: "stable-tool-call".into(),
                    name: "effect".into(),
                    arguments: json!({"value":"猫\nexact"}),
                }],
                provider_state: Some(
                    json!({"opaque":"signed-provider-data", "thinking":"retained"}),
                ),
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "complete answer\n猫".into(),
            usage: Some(Usage {
                input_tokens: 17,
                output_tokens: 5,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

struct Effect(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Effect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "effect".into(),
            description: "scripted external effect".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Approval::None,
        }
    }
    async fn invoke(&self, _: ToolContext, input: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult::text(format!(
            "{}\n{}",
            input["value"].as_str().unwrap(),
            "large exact tool output\n".repeat(900)
        )))
    }
}

fn harness(
    store: &Store,
    model: Arc<Model>,
    requests: Arc<Requests>,
    calls: Arc<AtomicUsize>,
) -> Result<Harness> {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Effect(calls)))?;
    Ok(Harness::new(
        store.clone(),
        model,
        tools,
        HarnessConfig {
            context: requests,
            ..Default::default()
        },
    ))
}

#[tokio::test]
async fn scripted_tool_turn_reopens_after_physical_serving_delete_with_exact_next_request()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let (session, original, prepared, dispatched, old_position) = {
        let store = Store::open_data_dir(directory.path())?;
        let session = store.create_session("scripted tool turn")?;
        let agent = harness(
            &store,
            Arc::new(Model::default()),
            Arc::new(Requests::default()),
            calls.clone(),
        )?;
        let run = agent
            .prompt(&session.id, "original\nrequest 猫", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let original = serde_json::to_vec(&store.messages(&session.id)?)?;
        assert_eq!(store.messages(&session.id)?.len(), 4);
        let chat = chat_path(&store, &session.id)?;
        for table in ["sessions", "messages", "blobs", "effects"] {
            assert!(has_table(&chat, table)?, "record table {table}");
        }
        for table in ["events", "context_views", "telemetry"] {
            assert!(
                !has_table(&chat, table)?,
                "disposable {table} must not live in chat"
            );
        }
        assert!(has_table(&directory.path().join("serving.db"), "events")?);
        assert!(rows(&directory.path().join("serving.db"), "events")? > 0);
        let old_position = agent.last_sequence();
        drop(agent);
        let requests = Arc::new(Requests::default());
        let model = Arc::new(Model {
            stop_after_capture: true,
            ..Default::default()
        });
        let probe = harness(&store, model.clone(), requests.clone(), calls.clone())?;
        assert_eq!(
            probe
                .resume(&session.id, RunOptions::default())
                .await?
                .status,
            "failed"
        );
        assert_eq!(serde_json::to_vec(&store.messages(&session.id)?)?, original);
        let prepared = requests.0.lock().unwrap()[0].clone();
        let dispatched = model.requests.lock().unwrap()[0].clone();
        drop(probe);
        store.idle_checkpoint()?;
        (session, original, prepared, dispatched, old_position)
    };
    std::fs::remove_file(directory.path().join("serving.db"))?;
    let store = Store::open_data_dir(directory.path())?;
    assert_eq!(serde_json::to_vec(&store.messages(&session.id)?)?, original);
    let requests = Arc::new(Requests::default());
    let model = Arc::new(Model {
        stop_after_capture: true,
        ..Default::default()
    });
    let probe = harness(&store, model.clone(), requests.clone(), calls.clone())?;
    assert!(
        !probe.can_resume_from(old_position),
        "the removed journal requires client resync"
    );
    assert_eq!(
        probe
            .resume(&session.id, RunOptions::default())
            .await?
            .status,
        "failed"
    );
    assert_eq!(requests.0.lock().unwrap()[0], prepared);
    let mut after = model.requests.lock().unwrap()[0].clone();
    // A fresh Harness has a new cache epoch and no previous tail; all other bytes match.
    after.cache_tiers = dispatched.cache_tiers;
    assert_ne!(after.history.epoch, dispatched.history.epoch);
    after.history.epoch = dispatched.history.epoch.clone();
    assert_eq!(
        serde_json::to_vec(&after)?,
        serde_json::to_vec(&dispatched)?
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    Ok(())
}

#[tokio::test]
async fn failed_serving_tool_journal_write_keeps_record_result_and_refuses_gap_resume() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let (session, history, failed_position) = {
        let store = Store::open_data_dir(directory.path())?;
        let session = store.create_session("journal failure")?;
        let serving = Connection::open(directory.path().join("serving.db"))?;
        serving.execute_batch("CREATE TRIGGER reject_result_journal BEFORE INSERT ON events WHEN NEW.kind='tool_finished' BEGIN SELECT RAISE(ABORT,'synthetic journal failure'); END;")?;
        let agent = harness(
            &store,
            Arc::new(Model::default()),
            Arc::new(Requests::default()),
            calls.clone(),
        )?;
        let before = agent.last_sequence();
        let mut subscription = agent.subscribe();
        let run = agent
            .prompt(
                &session.id,
                "accepted before journal failure",
                RunOptions::default(),
            )
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let history = store.messages(&session.id)?;
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].content, "accepted before journal failure");
        assert_eq!(history[2].role, Role::Tool);
        assert!(history[2].content.contains("large exact tool output"));
        assert_eq!(history[3].content, "complete answer\n猫");
        assert!(!agent.can_resume_from(before));
        assert!(
            agent
                .events_after(before, Some(&session.id), 100)?
                .iter()
                .all(|event| !matches!(event.kind, EventKind::ToolFinished { .. }))
        );
        let mut failed_position = None;
        while let Ok(event) = subscription.try_recv() {
            if matches!(event.kind, EventKind::ToolFinished { .. }) {
                assert!(
                    !event.durable,
                    "failed journal notification requires snapshot resync"
                );
                failed_position = Some(event.sequence);
            }
        }
        let failed_position = failed_position.expect("tool completion is still presented live");
        let chat = Connection::open(chat_path(&store, &session.id)?)?;
        assert_eq!(
            chat.query_row("SELECT state FROM effects", [], |r| r.get::<_, String>(0))?,
            "completed"
        );
        serving.execute_batch("DROP TRIGGER reject_result_journal")?;
        drop(chat);
        drop(serving);
        drop(agent);
        store.idle_checkpoint()?;
        (session, history, failed_position)
    };
    let store = Store::open_data_dir(directory.path())?;
    assert_eq!(store.messages(&session.id)?, history);
    let agent = harness(
        &store,
        Arc::new(Model {
            stop_after_capture: true,
            ..Default::default()
        }),
        Arc::new(Requests::default()),
        calls.clone(),
    )?;
    assert!(!agent.can_resume_from(failed_position.saturating_sub(1)));
    assert_eq!(
        agent
            .resume(&session.id, RunOptions::default())
            .await?
            .status,
        "failed"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(store.messages(&session.id)?, history);
    Ok(())
}

#[test]
fn parent_child_grandchild_share_a_chat_and_catalog_rebuild_preserves_sessions() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (original, original_catalog, paths) = {
        let store = Store::open_data_dir(directory.path())?;
        let root = store.create_session("root")?;
        let child = store.create_session_with_parent(
            "child",
            ContextStrategy::Classical,
            Some(&root.id),
        )?;
        let grandchild = store.create_session_with_parent(
            "grandchild",
            ContextStrategy::Lcm,
            Some(&child.id),
        )?;
        let unrelated = store.create_session("unrelated")?;
        let paths = [
            chat_path(&store, &root.id)?,
            chat_path(&store, &child.id)?,
            chat_path(&store, &grandchild.id)?,
            chat_path(&store, &unrelated.id)?,
        ];
        assert_eq!(paths[0], paths[1]);
        assert_eq!(paths[1], paths[2]);
        assert_ne!(paths[0], paths[3]);
        assert_eq!(
            paths[0].file_name().unwrap(),
            format!("{}.db", root.id).as_str()
        );
        assert_eq!(child.parent_session_id.as_deref(), Some(root.id.as_str()));
        assert_eq!(
            grandchild.parent_session_id.as_deref(),
            Some(child.id.as_str())
        );
        for session in [&root, &child, &grandchild, &unrelated] {
            store.append_batch(&[stored_message(
                &session.id,
                &format!("{}-message", session.id),
                Role::User,
                &session.title,
            )])?;
        }
        store.rename_session(&child.id, "renamed child")?;
        store.idle_checkpoint()?;
        let original = sorted_sessions(&store)?;
        assert_eq!(rows(&directory.path().join("catalog.db"), "sessions")?, 4);
        let original_catalog = catalog_rows(&directory.path().join("catalog.db"))?;
        assert!(
            original_catalog
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["id"] == child.id
                    && record["parent_id"] == root.id
                    && record["root_id"] == root.id)
        );
        assert!(
            original_catalog
                .as_array()
                .unwrap()
                .iter()
                .all(|record| record["storage_state"] == "active")
        );
        (original, original_catalog, paths)
    };
    std::fs::remove_file(directory.path().join("catalog.db"))?;
    let store = Store::open_data_dir(directory.path())?;
    assert_eq!(sorted_sessions(&store)?, original);
    assert_eq!(
        catalog_rows(&directory.path().join("catalog.db"))?,
        original_catalog
    );
    assert_eq!(store.catalog_rebuild()?, 4);
    assert_eq!(sorted_sessions(&store)?, original);
    assert_eq!(rows(&paths[0], "sessions")?, 3);
    Ok(())
}

fn sorted_sessions(store: &Store) -> Result<Value> {
    let mut sessions = store.list_sessions()?;
    sessions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(serde_json::to_value(sessions)?)
}

fn catalog_rows(path: &Path) -> Result<Value> {
    let conn = Connection::open(path)?;
    let mut stmt = conn.prepare("SELECT id,parent_id,root_id,title,last_activity,data,storage_state,created_at,file FROM sessions ORDER BY id")?;
    let records = stmt
        .query_map([], |row| {
            Ok(json!({
                "id":row.get::<_, String>(0)?, "parent_id":row.get::<_, Option<String>>(1)?,
                "root_id":row.get::<_, String>(2)?, "title":row.get::<_, String>(3)?,
                "last_activity":row.get::<_, String>(4)?, "data":row.get::<_, String>(5)?,
                "storage_state":row.get::<_, String>(6)?,
                "created_at":row.get::<_, String>(7)?, "file":row.get::<_, String>(8)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!(records))
}

/// Deterministic low-compressibility bytes make reclamation measurable even
/// though each blob is compressed. This is test content, not a timing workload.
fn unique_payload(seed: usize, len: usize) -> String {
    let mut content = String::with_capacity(len);
    let mut n = 0usize;
    while content.len() < len {
        content.push_str(&format!(
            "{:x}",
            Sha256::digest(format!("{seed}:{n}").as_bytes())
        ));
        n += 1;
    }
    content.truncate(len);
    content
}

#[test]
fn child_delete_reclaims_unique_blobs_but_retains_shared_content_and_other_root() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let root = store.create_session("root")?;
    let child = store.create_session_with_parent("child", ContextStrategy::Lcm, Some(&root.id))?;
    let grandchild =
        store.create_session_with_parent("grandchild", ContextStrategy::Lcm, Some(&child.id))?;
    let unrelated = store.create_session("unrelated")?;
    let shared = unique_payload(0, 96 * 1024);
    let retained = stored_message(&root.id, "retained-shared", Role::Tool, &shared);
    let copied = stored_message(&child.id, "child-shared", Role::Tool, &shared);
    store.append_batch(&[retained.clone(), copied])?;
    let mut doomed = Vec::new();
    for index in 1..=48 {
        doomed.push(stored_message(
            &child.id,
            &format!("unique-{index}"),
            Role::Tool,
            &unique_payload(index, 48 * 1024),
        ));
    }
    doomed.push(stored_message(
        &grandchild.id,
        "grandchild-unique",
        Role::Tool,
        &unique_payload(99, 128 * 1024),
    ));
    store.append_batch(&doomed)?;
    let untouched = stored_message(
        &unrelated.id,
        "untouched",
        Role::User,
        "another root survives",
    );
    store.append_batch(std::slice::from_ref(&untouched))?;
    let chat = chat_path(&store, &root.id)?;
    let other_chat = chat_path(&store, &unrelated.id)?;
    store.idle_checkpoint()?;
    let before = std::fs::metadata(&chat)?.len();
    let conn = Connection::open(&chat)?;
    assert_eq!(
        conn.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))?,
        2
    );
    assert_eq!(rows(&chat, "blobs")?, 50);
    drop(conn);
    store.delete_session(&child.id)?;
    store.idle_checkpoint()?;
    assert!(store.session(&child.id).is_err());
    assert!(store.session(&grandchild.id).is_err());
    assert!(chat.exists());
    assert_eq!(store.messages(&root.id)?, vec![retained]);
    assert_eq!(store.messages(&unrelated.id)?, vec![untouched]);
    assert_eq!(rows(&chat, "blobs")?, 1);
    let after = std::fs::metadata(&chat)?.len();
    assert!(
        after < before / 2,
        "child deletion should reclaim its large unique content: before={before}, after={after}"
    );
    store.delete_session(&root.id)?;
    assert!(!chat.exists(), "root deletion removes its record file");
    assert_eq!(store.list_sessions()?.len(), 1);
    assert!(other_chat.exists());
    Ok(())
}

#[test]
fn jsonl_exports_exact_blob_backed_messages_and_child_links() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (root, child, grandchild, expected_root, expected_child, expected_grandchild) = {
        let store = Store::open_data_dir(directory.path())?;
        let root = store.create_session("root")?;
        let child =
            store.create_session_with_parent("child", ContextStrategy::Lcm, Some(&root.id))?;
        let grandchild = store.create_session_with_parent(
            "grandchild",
            ContextStrategy::Lcm,
            Some(&child.id),
        )?;
        let mut assistant = stored_message(&root.id, "call", Role::Assistant, "call text\n猫");
        assistant.tool_calls.push(ToolCall {
            id: "call-id".into(),
            name: "effect".into(),
            arguments: json!({"literal":"\\\"\n猫"}),
        });
        assistant.provider_state =
            Some(json!({"thinking":"private opaque text", "signature":"signature exact"}));
        let mut result = stored_message(
            &root.id,
            "result",
            Role::Tool,
            &"exact output\n猫\\\"".repeat(700),
        );
        result.tool_call_id = Some("call-id".into());
        let child_message = stored_message(
            &child.id,
            "child-message",
            Role::Tool,
            &unique_payload(3, 9000),
        );
        let grandchild_message = stored_message(
            &grandchild.id,
            "grandchild-message",
            Role::User,
            "grandchild exact content 猫",
        );
        store.append_batch(&[
            assistant.clone(),
            result.clone(),
            child_message.clone(),
            grandchild_message.clone(),
        ])?;
        let mut root_export = Vec::new();
        let mut child_export = Vec::new();
        let mut grandchild_export = Vec::new();
        assert_eq!(store.export_jsonl(&root.id, &mut root_export)?, 2);
        assert_eq!(store.export_jsonl(&child.id, &mut child_export)?, 1);
        assert_eq!(
            store.export_jsonl(&grandchild.id, &mut grandchild_export)?,
            1
        );
        let empty = store.create_session("empty export")?;
        let mut empty_export = Vec::new();
        assert_eq!(store.export_jsonl(&empty.id, &mut empty_export)?, 0);
        assert!(empty_export.is_empty());
        assert_export(
            &root_export,
            &root,
            &[assistant, result],
            &root.id,
            Some(&child.id),
        )?;
        assert_export(
            &child_export,
            &child,
            &[child_message],
            &root.id,
            Some(&grandchild.id),
        )?;
        assert_export(
            &grandchild_export,
            &grandchild,
            &[grandchild_message],
            &root.id,
            None,
        )?;
        store.idle_checkpoint()?;
        (
            root,
            child,
            grandchild,
            root_export,
            child_export,
            grandchild_export,
        )
    };
    let mut root_export = Vec::new();
    let mut child_export = Vec::new();
    let mut grandchild_export = Vec::new();
    Store::export_data_dir_jsonl(directory.path(), &root.id, &mut root_export)?;
    Store::export_data_dir_jsonl(directory.path(), &child.id, &mut child_export)?;
    Store::export_data_dir_jsonl(directory.path(), &grandchild.id, &mut grandchild_export)?;
    assert_eq!(root_export, expected_root);
    assert_eq!(child_export, expected_child);
    assert_eq!(grandchild_export, expected_grandchild);
    Ok(())
}

fn assert_export(
    bytes: &[u8],
    session: &Session,
    expected: &[Message],
    root_id: &str,
    child: Option<&str>,
) -> Result<()> {
    let records: Vec<Value> = std::str::from_utf8(bytes)?
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<_>>()?;
    assert_eq!(records.len(), expected.len(), "one message per JSONL line");
    for record in &records {
        assert_eq!(record["session_id"], session.id);
        assert_eq!(
            record["parent_session_id"],
            serde_json::to_value(&session.parent_session_id)?
        );
        assert_eq!(record["root_id"], root_id);
    }
    let messages: Vec<Message> = records
        .iter()
        .filter_map(|record| record.get("message"))
        .map(|message| serde_json::from_value(message.clone()))
        .collect::<serde_json::Result<_>>()?;
    assert_eq!(messages, expected);
    if let Some(child) = child {
        assert!(
            records[0]["child_session_ids"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == child),
            "root export links its child"
        );
    }
    Ok(())
}

#[test]
fn deleting_serving_keeps_classical_and_lcm_derived_records_and_original_history() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (classical, lcm, original_classical, original_lcm, checkpoint, episode, chapter) = {
        let store = Store::open_data_dir(directory.path())?;
        let classical =
            store.create_session_with_strategy("classical", ContextStrategy::Classical)?;
        let lcm = store.create_session("lcm")?;
        let original_classical = vec![stored_message(
            &classical.id,
            "classical-source",
            Role::User,
            "classical canonical words 猫",
        )];
        let original_lcm = vec![stored_message(
            &lcm.id,
            "lcm-source",
            Role::User,
            "lcm canonical words 猫",
        )];
        store.append_batch(&original_classical)?;
        store.append_batch(&original_lcm)?;
        let checkpoint = ContextCheckpoint {
            id: "classical-summary".into(),
            session_id: classical.id.clone(),
            through_message_id: original_classical[0].id.clone(),
            model: "synthetic".into(),
            summary: "saved classical decision\nUnicode 猫".into(),
            files_touched: vec!["src/main.rs".into()],
            created_at: DATE.into(),
            usage: None,
        };
        store.save_context_checkpoint(&checkpoint)?;
        let chapter = ContextSubjectChapter {
            id: "lcm-chapter".into(),
            session_id: lcm.id.clone(),
            ordinal: 1,
            first_episode_ordinal: 1,
            last_episode_ordinal: 1,
            status: SubjectChapterStatus::Open,
            description: None,
            summary: None,
            related_ids: vec![],
            related_evidence: vec![],
            decisions: None,
            sections: None,
            model: None,
            usage: None,
            reasoning: None,
        };
        let episode = ContextEpisode {
            id: "lcm-episode".into(),
            session_id: lcm.id.clone(),
            ordinal: 1,
            first_message_id: original_lcm[0].id.clone(),
            last_message_id: original_lcm[0].id.clone(),
            source_tokens: 100,
            cap: 50,
            origin_request_id: None,
            title: "saved episode".into(),
            summary: "saved LCM decision\nUnicode 猫".into(),
            model: "synthetic".into(),
            usage: None,
            reasoning: Some("saved decision reasoning".into()),
            created_at: DATE.into(),
            chapter_id: chapter.id.clone(),
        };
        store.save_episode_batch(&EpisodeBatch {
            episode: episode.clone(),
            decisions: vec![],
            open_chapter: chapter.clone(),
            closed_chapter: None,
            close_after_episode: None,
            expected_previous_coverage: None,
        })?;
        for (session, summary) in [
            (&classical, checkpoint.summary.as_str()),
            (&lcm, episode.summary.as_str()),
        ] {
            let view = ContextView {
                session_id: session.id.clone(),
                frontier: Some(
                    store.message_seq(&session.id, &store.messages(&session.id)?[0].id)?,
                ),
                system_prompt: "saved serving instructions".into(),
                prefix: vec![stored_message(
                    &session.id,
                    &format!("{}-render", session.id),
                    Role::System,
                    summary,
                )],
                masked_tool_output_ids: vec![],
                clipped_tool_call_ids: vec![],
                created_at: DATE.into(),
                threshold: 200,
                budget: 100,
                tokens: 42,
                blocks: Vec::new(),
                blocks_estimated: true,
            };
            store.save_context_view(&view)?;
            assert!(store.context_view(&session.id)?.is_some());
            assert!(!has_table(
                &chat_path(&store, &session.id)?,
                "context_views"
            )?);
        }
        store.idle_checkpoint()?;
        (
            classical,
            lcm,
            original_classical,
            original_lcm,
            checkpoint,
            episode,
            chapter,
        )
    };
    std::fs::remove_file(directory.path().join("serving.db"))?;
    let store = Store::open_data_dir(directory.path())?;
    assert_eq!(store.messages(&classical.id)?, original_classical);
    assert_eq!(store.messages(&lcm.id)?, original_lcm);
    assert_eq!(
        serde_json::to_vec(&store.context_checkpoint(&classical.id)?.unwrap())?,
        serde_json::to_vec(&checkpoint)?
    );
    let durable_episode = ContextEpisode {
        reasoning: None,
        usage: None,
        ..episode
    };
    assert_eq!(
        serde_json::to_vec(&store.context_episodes(&lcm.id)?)?,
        serde_json::to_vec(&vec![durable_episode])?
    );
    assert_eq!(
        serde_json::to_vec(&store.context_subject_chapters(&lcm.id)?)?,
        serde_json::to_vec(&vec![chapter])?
    );
    // Hosts rebuild serving views from the preserved strategy records. The core
    // does not resurrect arbitrary disposable view settings.
    assert!(store.context_view(&classical.id)?.is_none());
    assert!(store.context_view(&lcm.id)?.is_none());
    Ok(())
}

struct ReusedCall;
#[async_trait]
impl Provider for ReusedCall {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            tool_calls: vec![ToolCall {
                id: "unknown-call".into(),
                name: "effect".into(),
                arguments: json!({"value":"must not run"}),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn pending_input_approval_record_and_unknown_effect_survive_serving_loss_without_replay()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let queued_text = "pending accepted user text\n猫\\\"".repeat(500);
    let (session, chat, original, input) = {
        let store = Store::open_data_dir(directory.path())?;
        let session = store.create_session("interrupted")?;
        let mut call = stored_message(
            &session.id,
            "unknown-assistant",
            Role::Assistant,
            "before effect",
        );
        call.tool_calls.push(ToolCall {
            id: "unknown-call".into(),
            name: "effect".into(),
            arguments: json!({"value":"must not run"}),
        });
        store.append_batch(&[call])?;
        let input = store.queue(&session.id, InputKind::Followup, &queued_text)?;
        store.idle_checkpoint()?;
        (
            session.clone(),
            chat_path(&store, &session.id)?,
            store.messages(&session.id)?,
            input,
        )
    };
    // A crash fixture at the durable dispatch boundary: no complete result was
    // stored. This avoids performing or interrupting any real external effect.
    let conn = Connection::open(&chat)?;
    let run = json!({"id":"crashed-run","session_id":session.id,"model":"synthetic","status":"running","text":null,"error":null,"started_at":DATE,"ended_at":null,"usage":null});
    conn.execute(
        "INSERT INTO runs(id,session_id,status,data) VALUES('crashed-run',?1,'running',?2)",
        params![session.id, run.to_string()],
    )?;
    conn.execute("INSERT INTO effects(id,session_id,run_id,call_id,state,detail) VALUES('unknown-effect',?1,'crashed-run','unknown-call','dispatched',?2)", params![session.id, json!({"id":"unknown-call","name":"effect","arguments":{"value":"must not run"}}).to_string()])?;
    conn.execute("INSERT INTO approvals(id,session_id,run_id,call_id,tool,arguments,state,created_at) VALUES('pending-approval',?1,'crashed-run','approval-call','effect',?2,'pending',?3)", params![session.id, json!({"value":"exact approval arguments 猫"}).to_string(), DATE])?;
    conn.execute(
        "INSERT INTO approval_rules(session_id,tool) VALUES(?1,'effect')",
        [&session.id],
    )?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    std::fs::remove_file(directory.path().join("serving.db"))?;
    let store = Store::open_data_dir(directory.path())?;
    let recovered = store.messages(&session.id)?;
    assert_eq!(&recovered[..original.len()], original.as_slice());
    assert_eq!(recovered.len(), original.len() + 1);
    assert_eq!(
        recovered.last().unwrap().tool_call_id.as_deref(),
        Some("unknown-call")
    );
    assert!(recovered.last().unwrap().is_error);
    assert!(
        recovered
            .last()
            .unwrap()
            .content
            .contains("outcome unknown")
    );
    assert_eq!(
        serde_json::to_vec(&store.queued_inputs(&session.id)?[0])?,
        serde_json::to_vec(&input)?
    );
    assert_eq!(store.queued_inputs(&session.id)?[0].text, queued_text);
    assert_eq!(store.run("crashed-run")?.status, "interrupted");
    let effects = store.pending_effects(&session.id)?;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0]["state"], "unknown");
    assert_eq!(
        effects[0]["call"]["arguments"],
        json!({"value":"must not run"})
    );
    store.flush_serving()?;
    let approval_path = if rows(&chat, "approvals")? == 0 {
        directory.path().join("serving.db")
    } else {
        chat.clone()
    };
    let conn = Connection::open(approval_path)?;
    let (approval_state, reason, arguments): (String, String, String) = conn.query_row(
        "SELECT state,reason,arguments FROM approvals WHERE id='pending-approval'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!(approval_state, "expired");
    assert_eq!(reason, "restart");
    assert_eq!(
        serde_json::from_str::<Value>(&arguments)?,
        json!({"value":"exact approval arguments 猫"})
    );
    assert_eq!(rows(&chat, "approval_rules")?, 1);
    drop(conn);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Effect(calls.clone())))?;
    let agent = Harness::new(
        store.clone(),
        Arc::new(ReusedCall),
        tools,
        HarnessConfig::default(),
    );
    let run = agent.resume(&session.id, RunOptions::default()).await?;
    assert_eq!(run.status, "failed");
    assert!(run.error.as_deref().unwrap().contains("reused"));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let resumed = store.messages(&session.id)?;
    assert_eq!(&resumed[..recovered.len()], recovered.as_slice());
    assert_eq!(resumed.len(), recovered.len() + 1);
    assert_eq!(resumed.last().unwrap().role, Role::User);
    assert_eq!(resumed.last().unwrap().content, queued_text);
    assert!(
        store
            .queued_inputs(&session.id)?
            .iter()
            .all(|input| input.state != "pending")
    );
    assert_eq!(store.pending_effects(&session.id)?[0]["state"], "unknown");
    Ok(())
}

#[path = "storage_layout/retention.rs"]
mod retention;
