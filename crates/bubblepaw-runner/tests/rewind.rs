use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, MessageOrigin, ModelRequest, ModelResponse,
    Provider, Role, RunOptions, Store, TextCallback, ToolCall, ToolRegistry, is_typed_user,
};
use bubblepaw_runner::{
    agents::AgentRegistry,
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::Agents,
    profiles::child_agent_host,
    rewind::{install_checkpoint_sweep, rewind_command, rewind_session},
    tools::{WorkspaceTools, register_native_tools},
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const PREFIX: &str = bubblepaw_core::CHECKPOINT_PREFIX;

fn register_js(tools: &mut ToolRegistry, root: &std::path::Path, data: &std::path::Path) {
    let artifacts = Arc::new(ArtifactStore::new(root, data.join("artifacts")).unwrap());
    Arc::new(CodeMode::new(root.to_owned(), artifacts).unwrap())
        .register(tools)
        .unwrap();
}

struct Edits;
#[async_trait]
impl Provider for Edits {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let last = request.messages.last().unwrap();
        if last.role == Role::User {
            // A host notice (budget) may follow the typed request; act on the typed row.
            let typed = request
                .messages
                .iter()
                .rev()
                .find(|message| is_typed_user(message))
                .unwrap_or(last);
            let input: Value = serde_json::from_str(&typed.content)?;
            let calls = vec![ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "js".into(),
                arguments: json!({"code":input["code"].as_str().unwrap()}),
            }];
            Ok(ModelResponse {
                tool_calls: calls,
                ..Default::default()
            })
        } else {
            Ok(ModelResponse {
                text: "done".into(),
                ..Default::default()
            })
        }
    }
}

struct Fixture {
    root: tempfile::TempDir,
    _data: tempfile::TempDir,
    store: Store,
    harness: Harness,
    session: String,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = Store::open_data_dir(data.path()).unwrap();
        install_checkpoint_sweep(&store).unwrap();
        let session = store.create_session("rewind source").unwrap().id;
        Self::attach(root, data, store, session)
    }

    /// A fixture over an already-open store: a fresh harness, so new turns can run
    /// after the store was reopened.
    fn attach(
        root: tempfile::TempDir,
        data: tempfile::TempDir,
        store: Store,
        session: String,
    ) -> Self {
        let mut tools = ToolRegistry::new();
        WorkspaceTools::new(root.path())
            .unwrap()
            .register(&mut tools, false)
            .unwrap();
        let artifacts =
            Arc::new(ArtifactStore::new(root.path(), data.path().join("artifacts")).unwrap());
        Arc::new(CodeMode::new(root.path().to_owned(), artifacts).unwrap())
            .register(&mut tools)
            .unwrap();
        let harness = Harness::new(
            store.clone(),
            Arc::new(Edits),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Self {
            root,
            _data: data,
            store,
            harness,
            session,
        }
    }
    async fn input(&self, session: &str, input: Value) {
        let run = self
            .harness
            .prompt(session, input.to_string(), RunOptions::default())
            .await
            .unwrap();
        assert!(run.error.is_none(), "{:?}", run.error);
        let messages = self.store.messages(session).unwrap();
        for message in messages.iter().filter(|message| message.role == Role::Tool) {
            assert!(!message.is_error, "{}", message.content);
        }
    }
    async fn edit(&self, path: &str, old_string: &str, new_string: &str) {
        self.input(&self.session, json!({"code":format!("return await tools.edit({});", json!({"path":path,"old_string":old_string,"new_string":new_string}))})).await;
    }
    async fn write(&self, path: &str, content: &str) {
        self.input(&self.session, json!({"code":format!("return await tools.write({});", json!({"path":path,"content":content}))})).await;
    }
    fn first_target(&self) -> String {
        self.store
            .messages(&self.session)
            .unwrap()
            .first()
            .unwrap()
            .id
            .clone()
    }
    /// The latest typed user message: the only valid rewind target.
    fn latest_target(&self) -> String {
        self.store
            .messages(&self.session)
            .unwrap()
            .into_iter()
            .rfind(is_typed_user)
            .unwrap()
            .id
    }
    fn records(&self, session: &str) -> Vec<(String, String)> {
        self.store
            .checkpoint_records_page(session, None, 1000)
            .unwrap()
    }
}

#[test]
fn slash_command_requires_exact_name() {
    assert_eq!(rewind_command(" /rewind abc "), Some("abc"));
    assert_eq!(rewind_command("/rewind"), Some(""));
    assert_eq!(rewind_command("/rewind\tabc"), Some("abc"));
    assert_eq!(rewind_command("/rewinder abc"), None);
    assert_eq!(rewind_command("please /rewind abc"), None);
}

#[tokio::test]
async fn direct_tool_invocation_cannot_bypass_durable_checkpoints() {
    let root = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::new();
    WorkspaceTools::new(root.path())
        .unwrap()
        .register(&mut tools, false)
        .unwrap();
    let context = bubblepaw_core::ToolContext {
        session_id: "direct".into(),
        run_id: "run".into(),
        invocation_id: "call".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let error = tools
        .get("write")
        .unwrap()
        .invoke(context, json!({"path":"a","content":"unsafe\n"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("dispatcher"));
    assert!(!root.path().join("a").exists());
}

#[tokio::test]
async fn unreadable_checkpoint_metadata_aborts_edit_before_filesystem_effect() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.store
        .write_checkpoint(
            &f.session,
            bubblepaw_core::CheckpointRecord {
                id: "file-checkpoint-v1/00000000000000000001/000000",
                patch_index: 1,
                boundary_seq: 0,
                metadata: "invalid metadata",
            },
            b"",
            &[],
        )
        .unwrap();
    let run =f.harness.prompt(&f.session, json!({"code":format!("return await tools.edit({});", json!({"path":"a","old_string":"one","new_string":"two"}))}).to_string(), RunOptions::default()).await.unwrap();
    assert!(run.error.is_none());
    let tools: Vec<_> = f
        .store
        .messages(&f.session)
        .unwrap()
        .into_iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert!(tools[0].is_error);
    assert!(tools[0].content.contains("checkpoint metadata"));
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"one\n");
}

#[tokio::test]
async fn durable_unexecuted_intent_conflicts_instead_of_restoring_uncertain_content() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    let records = f.records(&f.session);
    let mut metadata: Value = serde_json::from_str(&records[0].1).unwrap();
    metadata["patch_index"] = json!(2);
    metadata["pre_hash"] = metadata["post_hash"].clone();
    metadata["post_hash"] = json!(bubblepaw_core::fingerprint(b"three\n"));
    f.store
        .write_checkpoint(
            &f.session,
            bubblepaw_core::CheckpointRecord {
                id: "file-checkpoint-v1/00000000000000000002/000000",
                patch_index: 2,
                boundary_seq: metadata["boundary_seq"].as_i64().unwrap(),
                metadata: &metadata.to_string(),
            },
            b"two\n",
            &[],
        )
        .unwrap();
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.first_target()).unwrap();
    assert_eq!(result.conflicts.len(), 1);
    assert!(result.conflicts[0].reason.contains("postimage"));
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"two\n");
}

#[cfg(unix)]
#[tokio::test]
async fn replacement_symlink_conflicts_without_touching_its_target() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    fs::write(outside.path().join("outside"), "two\n").unwrap();
    fs::remove_file(f.root.path().join("a")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("outside"), f.root.path().join("a")).unwrap();
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.first_target()).unwrap();
    assert_eq!(result.conflicts.len(), 1);
    assert!(result.conflicts[0].reason.contains("symlink"));
    assert_eq!(fs::read(outside.path().join("outside")).unwrap(), b"two\n");
}

#[tokio::test]
async fn rewind_multiple_edits_and_writes_preserve_source_and_fork_prefix() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    // Earlier turn: changes here are older than the latest user message and
    // must survive a rewind to that message.
    f.edit("a", "one", "two").await;
    f.write("new", "created\n").await;
    f.write("nested/created", "nested content\n").await;
    // Latest turn: two checkpointed edits in one run.
    f.input(
        &f.session,
        json!({"code": format!(
            "await tools.edit({}); return await tools.edit({});",
            json!({"path":"a","old_string":"two","new_string":"three"}),
            json!({"path":"new","old_string":"created","new_string":"changed"}),
        )}),
    )
    .await;
    let before = f.store.messages(&f.session).unwrap();
    let before_records = f.records(&f.session);
    let target = f.latest_target();
    let result = rewind_session(&f.store, f.root.path(), &f.session, &target).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(result.restored, ["a", "new"]);
    assert!(result.deleted.is_empty());
    assert!(result.recreated.is_empty());
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"two\n");
    assert_eq!(fs::read(f.root.path().join("new")).unwrap(), b"created\n");
    assert_eq!(
        fs::read(f.root.path().join("nested/created")).unwrap(),
        b"nested content\n"
    );
    assert_eq!(
        serde_json::to_value(f.store.messages(&f.session).unwrap()).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    assert_eq!(f.records(&f.session), before_records);
    let fork = f.store.messages(&result.session_id).unwrap();
    let target_content = before
        .iter()
        .find(|message| message.id == target)
        .unwrap()
        .content
        .clone();
    assert_eq!(fork.last().unwrap().content, target_content);
    assert_ne!(fork.last().unwrap().id, target);
    assert!(f.records(&result.session_id).is_empty());
    assert!(result.limitation.contains("Exec"));
}

#[cfg(windows)]
#[tokio::test]
async fn legacy_windows_checkpoint_paths_share_the_chain_with_new_slash_paths() {
    let f = Fixture::new();
    fs::create_dir(f.root.path().join("nested")).unwrap();
    fs::write(f.root.path().join("nested/a"), "one\n").unwrap();
    f.edit("nested/a", "one", "two").await;
    // Latest turn: two checkpoints for the same path in one run. The first is
    // rewritten to the legacy spelling below.
    f.input(
        &f.session,
        json!({"code": format!(
            "await tools.edit({}); return await tools.edit({});",
            json!({"path":"nested/a","old_string":"two","new_string":"three"}),
            json!({"path":"nested/a","old_string":"three","new_string":"four"}),
        )}),
    )
    .await;
    let records = f.records(&f.session);
    let (id, metadata) = records[records.len() - 2].clone();
    let mut metadata: Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(metadata["path"], "nested/a");
    metadata["path"] = json!("nested\\a");
    // Emulate an existing checkpoint written by the previous Windows version.
    // Production checkpoint records remain immutable; only this historical fixture is
    // rewritten before the next ordinary edit appends its new-format record.
    let db = rusqlite::Connection::open(f.store.chat_path(&f.session).unwrap().unwrap()).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE checkpoint_records SET metadata=?1 WHERE session_id=?2 AND id=?3",
            rusqlite::params![metadata.to_string(), &f.session, id],
        )
        .unwrap(),
        1
    );
    let before_records = f.records(&f.session);
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.latest_target()).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(result.restored, ["nested/a"]);
    assert_eq!(fs::read(f.root.path().join("nested/a")).unwrap(), b"two\n");
    assert_eq!(f.records(&f.session), before_records);
}

#[cfg(unix)]
#[tokio::test]
async fn unix_literal_backslash_filename_survives_checkpoint_path_serialization() {
    let f = Fixture::new();
    let relative = r"nested/literal\name";
    f.write(relative, "created\n").await;
    let metadata: Value = serde_json::from_str(&f.records(&f.session)[0].1).unwrap();
    assert_eq!(metadata["path"], relative);
    assert_eq!(
        fs::read(f.root.path().join(relative)).unwrap(),
        b"created\n"
    );
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.first_target()).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(result.deleted, [relative]);
    assert!(!f.root.path().join(relative).exists());
}

#[tokio::test]
async fn same_assistant_and_nested_js_changes_keep_order_and_cutoff() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    let code = format!(
        "await tools.edit({}); return await tools.edit({});",
        json!({"path":"a","old_string":"one","new_string":"two"}),
        json!({"path":"a","old_string":"two","new_string":"three"})
    );
    f.input(&f.session, json!({"code":code})).await;
    let records = f.records(&f.session);
    assert_eq!(records.len(), 2);
    let a: Value = serde_json::from_str(&records[0].1).unwrap();
    let b: Value = serde_json::from_str(&records[1].1).unwrap();
    assert_eq!(a["boundary_message_id"], b["boundary_message_id"]);
    assert_eq!(a["patch_index"], 1);
    assert_eq!(b["patch_index"], 2);
    assert_ne!(a["invocation_id"], b["invocation_id"]);
    // A complete tool result is after both nested edits; including it keeps
    // their effects. A later user cutoff therefore rewinds only the later edit.
    let after = f
        .store
        .messages(&f.session)
        .unwrap()
        .into_iter()
        .find(|m| m.role == Role::Tool)
        .unwrap()
        .id;
    f.input(
        &f.session,
        json!({"code":format!("return await tools.edit({});", json!({"path":"a","old_string":"three","new_string":"four"}))}),
    )
    .await;
    // A tool-round boundary is no longer a target; only the latest user message is.
    let error = rewind_session(&f.store, f.root.path(), &f.session, &after).unwrap_err();
    assert!(format!("{error:#}").contains("only the last message can be rewound"));
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.latest_target()).unwrap();
    assert!(result.conflicts.is_empty());
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"three\n");
}

#[tokio::test]
async fn outside_edits_conflict_even_when_equal_to_original_preimage() {
    for outside in ["external\n", "one\n"] {
        let f = Fixture::new();
        fs::write(f.root.path().join("a"), "one\n").unwrap();
        f.edit("a", "one", "two").await;
        fs::write(f.root.path().join("a"), outside).unwrap();
        let result =
            rewind_session(&f.store, f.root.path(), &f.session, &f.first_target()).unwrap();
        assert_eq!(result.conflicts.len(), 1);
        assert!(result.restored.is_empty());
        assert_eq!(
            fs::read_to_string(f.root.path().join("a")).unwrap(),
            outside
        );
    }
}

#[tokio::test]
async fn outside_edit_before_the_latest_message_is_not_undone() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    fs::write(f.root.path().join("a"), "outside\n").unwrap();
    f.edit("a", "outside", "three").await;
    // The outside edit precedes the latest user message, so it is outside the window.
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.latest_target()).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(result.restored, ["a"]);
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"outside\n");
}

#[tokio::test]
async fn chain_break_inside_the_latest_turn_conflicts_without_restoring() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.input(
        &f.session,
        json!({"code": format!(
            "await tools.edit({}); return await tools.edit({});",
            json!({"path":"a","old_string":"one","new_string":"two"}),
            json!({"path":"a","old_string":"two","new_string":"three"}),
        )}),
    )
    .await;
    // Break the recorded chain between the two checkpoints of the latest turn.
    let (id, metadata) = f.records(&f.session).remove(1);
    let mut metadata: Value = serde_json::from_str(&metadata).unwrap();
    metadata["pre_hash"] = json!(bubblepaw_core::fingerprint(b"other\n"));
    let db = rusqlite::Connection::open(f.store.chat_path(&f.session).unwrap().unwrap()).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE checkpoint_records SET metadata=?1 WHERE session_id=?2 AND id=?3",
            rusqlite::params![metadata.to_string(), &f.session, id],
        )
        .unwrap(),
        1
    );
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.latest_target()).unwrap();
    assert_eq!(result.conflicts.len(), 1);
    assert!(result.conflicts[0].reason.contains("between patches"));
    assert!(result.restored.is_empty());
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"three\n");
}

#[tokio::test]
async fn wrong_workspace_rejects_before_fork_or_file_changes() {
    let f = Fixture::new();
    let wrong = tempfile::tempdir().unwrap();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    fs::write(wrong.path().join("a"), "unrelated\n").unwrap();
    f.edit("a", "one", "two").await;
    let before = f.store.list_sessions().unwrap().len();
    let error = rewind_session(&f.store, wrong.path(), &f.session, &f.first_target()).unwrap_err();
    assert!(error.to_string().contains("workspace"));
    assert_eq!(f.store.list_sessions().unwrap().len(), before);
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"two\n");
    assert_eq!(fs::read(wrong.path().join("a")).unwrap(), b"unrelated\n");
}

#[tokio::test]
async fn changed_checkpoint_boundary_rejects_before_fork_or_file_changes() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    let record = f.records(&f.session).remove(0);
    let mut metadata: Value = serde_json::from_str(&record.1).unwrap();
    metadata["boundary_seq"] = json!(metadata["boundary_seq"].as_i64().unwrap() + 1);
    metadata["patch_index"] = json!(2);
    f.store
        .write_checkpoint(
            &f.session,
            bubblepaw_core::CheckpointRecord {
                id: "file-checkpoint-v1/00000000000000000002/000000",
                patch_index: 2,
                boundary_seq: metadata["boundary_seq"].as_i64().unwrap(),
                metadata: &metadata.to_string(),
            },
            b"two\n",
            &[],
        )
        .unwrap();
    let before = f.store.list_sessions().unwrap().len();
    let error = rewind_session(&f.store, f.root.path(), &f.session, &f.first_target()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("boundary no longer matches source history")
    );
    assert_eq!(f.store.list_sessions().unwrap().len(), before);
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"two\n");
}

#[tokio::test]
async fn oversized_file_write_succeeds_but_durable_barrier_blocks_older_rewind() {
    let f = Fixture::new();
    let original = vec![b'x'; 10 * 1024 * 1024 + 1];
    fs::write(f.root.path().join("large"), &original).unwrap();
    f.write("large", "replaced").await;
    assert_eq!(fs::read(f.root.path().join("large")).unwrap(), b"replaced");
    let records = f.records(&f.session);
    let bytes = f
        .store
        .checkpoint_preimage(&f.session, &records[0].0)
        .unwrap()
        .unwrap();
    assert!(bytes.is_empty());
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.first_target()).unwrap();
    assert_eq!(result.conflicts.len(), 1);
    assert!(result.conflicts[0].reason.contains("10 MiB"));
    assert_eq!(fs::read(f.root.path().join("large")).unwrap(), b"replaced");
}

#[tokio::test]
async fn deduplicates_identical_preimages_and_prunes_deleted_session_records() {
    let f = Fixture::new();
    let child = f
        .store
        .create_session_with_parent("checkpoint owner", Default::default(), Some(&f.session))
        .unwrap();
    let original = "same content\n".repeat(2000);
    fs::write(f.root.path().join("a"), &original).unwrap();
    fs::write(f.root.path().join("b"), &original).unwrap();
    f.input(&child.id, json!({"code":"await tools.write({path:'a',content:'replaced'}); return await tools.write({path:'b',content:'replaced'});"})).await;
    let path = f.store.chat_path(&f.session).unwrap().unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let (records, blobs): (i64, i64) = db
        .query_row(
            "SELECT COUNT(*),COUNT(DISTINCT preimage_hash) FROM checkpoint_records",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((records, blobs), (2, 1));
    // The root retains the chat file while deleting the child removes its
    // attachments and orphan preimages, making physical pruning observable.
    f.store.delete_session(&child.id).unwrap();
    let remaining: i64 = db
        .query_row("SELECT COUNT(*) FROM checkpoint_records", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(remaining, 0);
    let orphan: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM checkpoint_blobs WHERE raw_bytes=?1",
            [i64::try_from(original.len()).unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orphan, 0);
    assert!(f.store.session(&f.session).is_ok());
}

#[tokio::test]
async fn archived_chat_retains_checkpoints_and_can_rewind_after_restore() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    let target = f.first_target();
    let records = f.records(&f.session);
    assert!(f.store.archive_chat(&f.session).unwrap());
    assert_eq!(f.records(&f.session), records);
    let result = rewind_session(&f.store, f.root.path(), &f.session, &target).unwrap();
    assert!(result.conflicts.is_empty());
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"one\n");
}

#[tokio::test]
async fn child_and_other_session_checkpoints_are_not_rewound() {
    let f = Fixture::new();
    fs::write(f.root.path().join("root"), "one\n").unwrap();
    f.edit("root", "one", "two").await;
    let child = f
        .store
        .create_session_with_parent("child", Default::default(), Some(&f.session))
        .unwrap();
    let other = f.store.create_session("other").unwrap();
    for (session, path) in [(&child.id, "child"), (&other.id, "other")] {
        f.input(session, json!({"code":format!("return await tools.write({});", json!({"path":path,"content":"keep\n"}))})).await;
    }
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.first_target()).unwrap();
    assert!(result.conflicts.is_empty());
    assert_eq!(result.restored, ["root"]);
    assert_eq!(fs::read(f.root.path().join("child")).unwrap(), b"keep\n");
    assert_eq!(fs::read(f.root.path().join("other")).unwrap(), b"keep\n");
}

#[tokio::test]
async fn bound_child_worktree_rewind_uses_its_checkout_and_isolates_same_relative_path() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "a"],
        vec![
            "-c",
            "user.name=Rewind Test",
            "-c",
            "user.email=rewind@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    ] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(f.root.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let child = f
        .store
        .create_session_with_parent("isolated child", Default::default(), Some(&f.session))
        .unwrap();
    let binding = bubblepaw_runner::worktrees::create(&f.store, f.root.path(), &child.id).unwrap();
    // Git may check out CRLF even though the committed parent fixture used LF.
    // The edit contract is literal, and rewind must restore these exact bytes.
    let child_original = fs::read(binding.path.join("a")).unwrap();
    let child_eol = match child_original.as_slice() {
        b"one\n" => "\n",
        b"one\r\n" => "\r\n",
        other => panic!("unexpected child checkout bytes: {other:?}"),
    };
    let child_changed = format!("child changed{child_eol}").into_bytes();
    let mut tools = ToolRegistry::new();
    WorkspaceTools::new(&binding.path)
        .unwrap()
        .register(&mut tools, false)
        .unwrap();
    register_js(&mut tools, &binding.path, f._data.path());
    let child_harness = Harness::new(
        f.store.clone(),
        Arc::new(Edits),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let run = child_harness.prompt(&child.id,
        json!({"code":format!("return await tools.edit({});", json!({"path":"a","old_string":format!("one{child_eol}"),"new_string":format!("child changed{child_eol}")}))}).to_string(),
        RunOptions::default()).await.unwrap();
    assert!(run.error.is_none(), "{:?}", run.error);
    for message in f
        .store
        .messages(&child.id)
        .unwrap()
        .into_iter()
        .filter(|message| message.role == Role::Tool)
    {
        assert!(!message.is_error, "{}", message.content);
    }
    assert_eq!(fs::read(binding.path.join("a")).unwrap(), child_changed);
    let child_target = f
        .store
        .messages(&child.id)
        .unwrap()
        .first()
        .unwrap()
        .id
        .clone();
    f.edit("a", "one", "root changed").await;
    let root_result =
        rewind_session(&f.store, f.root.path(), &f.session, &f.first_target()).unwrap();
    assert!(root_result.conflicts.is_empty());
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"one\n");
    assert_eq!(fs::read(binding.path.join("a")).unwrap(), child_changed);
    fs::write(f.root.path().join("a"), "keep root\n").unwrap();
    // Deliberately pass the root fallback: the durable binding must choose
    // the child's checkout and its canonical checkpoint root.
    let child_result = rewind_session(&f.store, f.root.path(), &child.id, &child_target).unwrap();
    assert!(
        child_result.conflicts.is_empty(),
        "{:?}",
        child_result.conflicts
    );
    assert_eq!(child_result.restored, ["a"]);
    assert_eq!(fs::read(binding.path.join("a")).unwrap(), child_original);
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"keep root\n");
}

#[tokio::test]
async fn fifty_edits_report_store_cost_and_compressed_preimages() {
    // Observe inside the real model/tool loop, before idle housekeeping truncates
    // the WAL. Between-turn samples alone would miss its transient growth.
    struct MeasuredEdits {
        store: Store,
        peak: Arc<AtomicU64>,
    }
    #[async_trait]
    impl Provider for MeasuredEdits {
        async fn generate(
            &self,
            request: ModelRequest,
            callback: TextCallback,
            cancellation: CancellationToken,
        ) -> Result<ModelResponse> {
            self.peak
                .fetch_max(self.store.storage_stats()?.wal_bytes, Ordering::Relaxed);
            Edits.generate(request, callback, cancellation).await
        }
    }
    let mut f = Fixture::new();
    let within_turn_peak = Arc::new(AtomicU64::new(0));
    let mut tools = ToolRegistry::new();
    WorkspaceTools::new(f.root.path())
        .unwrap()
        .register(&mut tools, false)
        .unwrap();
    register_js(&mut tools, f.root.path(), f._data.path());
    f.harness = Harness::new(
        f.store.clone(),
        Arc::new(MeasuredEdits {
            store: f.store.clone(),
            peak: within_turn_peak.clone(),
        }),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let padding = "x".repeat(20 * 1024 - 8);
    fs::write(f.root.path().join("a"), format!("mark00\n{padding}\n")).unwrap();
    f.store.idle_checkpoint().unwrap();
    let before = f.store.storage_stats().unwrap();
    let mut peak_wal = before.wal_bytes;
    for index in 0..50 {
        f.edit(
            "a",
            &format!("mark{index:02}"),
            &format!("mark{:02}", index + 1),
        )
        .await;
        peak_wal = peak_wal.max(f.store.storage_stats().unwrap().wal_bytes);
    }
    let after = f.store.storage_stats().unwrap();
    let db = rusqlite::Connection::open(f.store.chat_path(&f.session).unwrap().unwrap()).unwrap();
    let (records, blobs, raw, compressed): (i64,i64,i64,i64) = db.query_row(
        "SELECT COUNT(*),COUNT(DISTINCT a.preimage_hash),SUM(b.raw_bytes),SUM(length(b.data)) FROM checkpoint_records a JOIN checkpoint_blobs b ON b.hash=a.preimage_hash", [],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    f.store.idle_checkpoint().unwrap();
    let settled = f.store.storage_stats().unwrap();
    println!(
        "rewind_cost_50x20KiB {}",
        json!({"checkpoint_records":records,"checkpoint_blobs":blobs,"preimage_raw_bytes":raw,"preimage_compressed_bytes":compressed,"store_transaction_delta":after.transaction_count-before.transaction_count,"chat_transaction_delta":after.chat_transaction_count-before.chat_transaction_count,"catalog_transaction_delta":after.catalog_transaction_count-before.catalog_transaction_count,"serving_transaction_delta":after.serving_transaction_count-before.serving_transaction_count,"database_before":before.database_bytes,"database_after":after.database_bytes,"database_after_idle_checkpoint":settled.database_bytes,"page_before":before.page_bytes,"page_after":after.page_bytes,"chat_before":before.chat_bytes,"chat_after":after.chat_bytes,"chat_after_idle_checkpoint":settled.chat_bytes,"wal_before":before.wal_bytes,"wal_after":after.wal_bytes,"wal_after_idle_checkpoint":settled.wal_bytes,"wal_sampled_between_turns_peak":peak_wal,"wal_sampled_within_turn_peak":within_turn_peak.load(Ordering::Relaxed),"checkpoint_transactions_expected":50})
    );
    // Pruning keeps the current and previous turn only.
    assert_eq!((records, blobs), (2, 2));
    assert_eq!(raw, 2 * 20 * 1024);
    assert!(compressed < raw / 10);
    // Only the latest turn is rewindable: the last edit reverts to mark49.
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.latest_target()).unwrap();
    assert!(result.conflicts.is_empty());
    assert_eq!(
        fs::read_to_string(f.root.path().join("a")).unwrap(),
        format!("mark49\n{padding}\n")
    );
}

#[tokio::test]
async fn rewind_accepts_only_the_latest_typed_user_message() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    let first = f.first_target();
    f.edit("a", "two", "three").await;
    let latest = f.latest_target();
    assert_ne!(first, latest);
    let fork_count = |store: &Store| store.list_sessions().unwrap().len();
    let forks_before = fork_count(&f.store);

    // An older user message is rejected before any restore or fork.
    let error = rewind_session(&f.store, f.root.path(), &f.session, &first).unwrap_err();
    assert!(
        format!("{error:#}").contains("only the last message can be rewound"),
        "{error:#}"
    );
    // So is a non-user message from the latest turn.
    let assistant = f
        .store
        .messages(&f.session)
        .unwrap()
        .into_iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .unwrap()
        .id;
    let error = rewind_session(&f.store, f.root.path(), &f.session, &assistant).unwrap_err();
    assert!(format!("{error:#}").contains("only the last message can be rewound"));
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"three\n");
    assert_eq!(fork_count(&f.store), forks_before);

    // The latest user message restores its own turn.
    let result = rewind_session(&f.store, f.root.path(), &f.session, &latest).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(result.restored, ["a"]);
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"two\n");
}

#[tokio::test]
async fn host_notices_never_become_rewind_targets() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    let options = RunOptions {
        max_tool_calls: Some(10),
        ..RunOptions::default()
    };
    let run = f
        .harness
        .prompt(
            &f.session,
            json!({"code": format!("return await tools.edit({});", json!({"path":"a","old_string":"one","new_string":"two"}))}).to_string(),
            options,
        )
        .await
        .unwrap();
    assert!(run.error.is_none(), "{:?}", run.error);
    let latest = f.latest_target();
    // The budget notice is a generated user row written after the typed message.
    let notice = f
        .store
        .messages(&f.session)
        .unwrap()
        .into_iter()
        .find(|message| message.role == Role::User && !is_typed_user(message))
        .expect("budget notice is persisted")
        .id;
    let error = rewind_session(&f.store, f.root.path(), &f.session, &notice).unwrap_err();
    assert!(format!("{error:#}").contains("only the last message can be rewound"));
    let result = rewind_session(&f.store, f.root.path(), &f.session, &latest).unwrap();
    assert_eq!(result.restored, ["a"]);
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"one\n");
}

/// Run one child-task turn: the origin that agent children use for instructions.
async fn child_task(f: &Fixture, session: &str, code: &str) {
    let options = RunOptions {
        origin: MessageOrigin::ChildTask,
        ..RunOptions::default()
    };
    let run = f
        .harness
        .prompt(session, json!({ "code": code }).to_string(), options)
        .await
        .unwrap();
    assert!(run.error.is_none(), "{:?}", run.error);
    for message in f
        .store
        .messages(session)
        .unwrap()
        .into_iter()
        .filter(|message| message.role == Role::Tool)
    {
        assert!(!message.is_error, "{}", message.content);
    }
}

fn edit_code(path: &str, old_string: &str, new_string: &str) -> String {
    format!(
        "return await tools.edit({});",
        json!({"path": path, "old_string": old_string, "new_string": new_string})
    )
}

#[tokio::test]
async fn checkpoints_keep_only_the_current_and_previous_turn() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "0\n").unwrap();
    for n in 1..=4 {
        f.edit("a", &format!("{}\n", n - 1), &format!("{n}\n"))
            .await;
    }
    // Turns 1 and 2 are older than the previous turn: their records are gone.
    assert_eq!(f.records(&f.session).len(), 2);
    assert_eq!(fs::read_to_string(f.root.path().join("a")).unwrap(), "4\n");
    let first = f.first_target();
    let error = rewind_session(&f.store, f.root.path(), &f.session, &first).unwrap_err();
    assert!(format!("{error:#}").contains("only the last message can be rewound"));
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.latest_target()).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(fs::read_to_string(f.root.path().join("a")).unwrap(), "3\n");
}

#[tokio::test]
async fn numbering_stays_monotone_when_pruning_removes_earlier_records() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    // Two turns without checkpoints: turn 1's only record becomes stale.
    f.input(&f.session, json!({"code": "return 'noop';"})).await;
    f.input(&f.session, json!({"code": "return 'noop';"})).await;
    // This write prunes turn 1's record in the same transaction, and its new
    // record continues the numbering after the pruned one.
    f.edit("a", "two", "three").await;
    assert_eq!(f.records(&f.session).len(), 1);
    let result = rewind_session(&f.store, f.root.path(), &f.session, &f.latest_target()).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(
        fs::read_to_string(f.root.path().join("a")).unwrap(),
        "two\n"
    );
}

#[tokio::test]
async fn child_task_only_session_has_turns_and_rewinds_its_latest_turn() {
    let f = Fixture::new();
    let child = f
        .store
        .create_session_with_parent("child task", Default::default(), Some(&f.session))
        .unwrap();
    fs::write(f.root.path().join("child"), "one\n").unwrap();
    child_task(&f, &child.id, &edit_code("child", "one", "two")).await;
    child_task(&f, &child.id, &edit_code("child", "two", "three")).await;
    let messages = f.store.messages(&child.id).unwrap();
    let users: Vec<_> = messages
        .iter()
        .filter(|message| message.role == Role::User)
        .collect();
    assert_eq!(users.len(), 2);
    assert!(users.iter().all(|message| !is_typed_user(message)));
    assert_eq!(f.records(&child.id).len(), 2);
    let first = users[0].id.clone();
    let latest = users[1].id.clone();
    let error = rewind_session(&f.store, f.root.path(), &child.id, &first).unwrap_err();
    assert!(format!("{error:#}").contains("only the last message can be rewound"));
    let result = rewind_session(&f.store, f.root.path(), &child.id, &latest).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(result.restored, ["child"]);
    assert_eq!(
        fs::read_to_string(f.root.path().join("child")).unwrap(),
        "two\n"
    );
}

/// Drop every handle to the fixture's store and open the same data directory
/// again with the sweep installed, as a restart would. The root and data
/// directories are returned so they outlive the reopened store.
fn reopen(f: Fixture) -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let Fixture {
        root,
        _data,
        store,
        harness,
        session: _,
    } = f;
    drop((store, harness));
    let store = Store::open_data_dir(_data.path()).unwrap();
    install_checkpoint_sweep(&store).unwrap();
    (root, _data, store)
}

/// The fixture reopened: the chat opens again, which runs the sweep, and new turns
/// run on a fresh harness over the reopened store.
fn reopened(f: Fixture) -> Fixture {
    let session = f.session.clone();
    let (root, data, store) = reopen(f);
    Fixture::attach(root, data, store, session)
}

/// Insert a legacy checkpoint row under the checkpoint prefix in `attachments`, with
/// its preimage in `blobs`, as an older build left it, and return its ID and the
/// blob hash. The blob is left unreferenced by any plain row, as a leftover would be.
fn legacy_record(f: &Fixture, session: &str, bytes: &[u8]) -> (String, String) {
    let legacy = json!({
        "workspace": f.root.path().display().to_string(),
        "path": "legacy",
        "invocation_id": "legacy",
        "boundary_message_id": null,
        "boundary_seq": 0,
        "patch_index": 1,
        "pre_hash": null,
        "post_hash": null,
        "barrier": null,
    });
    let id = format!("{PREFIX}{:020}/{:06}", 0, 0);
    let hash = f
        .store
        .store_attachment(session, "legacy-blob", "{}", bytes)
        .unwrap();
    let path = f.store.chat_path(session).unwrap().unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute(
        "INSERT INTO attachments(session_id,id,metadata,blob_hash) VALUES(?1,?2,?3,?4)",
        rusqlite::params![session, id, legacy.to_string(), hash],
    )
    .unwrap();
    // A chat from the earlier build has no upgrade marker; the sweep sets it.
    db.execute(
        "DELETE FROM metadata WHERE key='rewind_checkpoint_prune'",
        [],
    )
    .unwrap();
    db.execute(
        "DELETE FROM attachments WHERE session_id=?1 AND id='legacy-blob'",
        [session],
    )
    .unwrap();
    (id, hash)
}

/// Legacy checkpoint rows still in `attachments` of a chat, counted in SQL.
fn legacy_rows(store: &Store, session: &str) -> i64 {
    let path = store.chat_path(session).unwrap().unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM attachments WHERE substr(id,1,length(?1))=?1",
            [PREFIX],
            |row| row.get(0),
        )
        .unwrap()
}

fn blob_rows(store: &Store, session: &str, hash: &str) -> i64 {
    let path = store.chat_path(session).unwrap().unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM blobs WHERE hash=?1", [hash], |row| {
            row.get(0)
        })
        .unwrap()
}

#[tokio::test]
async fn one_time_sweep_prunes_a_legacy_record_in_a_session_that_never_writes_again() {
    let f = Fixture::new();
    let child = f
        .store
        .create_session_with_parent("idle child", Default::default(), Some(&f.session))
        .unwrap();
    fs::write(f.root.path().join("child"), "one\n").unwrap();
    fs::write(f.root.path().join("r"), "x\n").unwrap();
    for (old, new) in [("one", "two"), ("two", "three"), ("three", "four")] {
        child_task(&f, &child.id, &edit_code("child", old, new)).await;
    }
    assert_eq!(f.records(&child.id).len(), 2);
    let (legacy_id, plain) = legacy_record(&f, &child.id, b"legacy preimage");
    assert_eq!(legacy_rows(&f.store, &child.id), 1);

    // The sweep runs when the chat opens in the next process, not in a write.
    let (_root, _data, store) = reopen(f);
    assert_eq!(
        store
            .chat_metadata(&child.id, "rewind_checkpoint_prune")
            .unwrap()
            .as_deref(),
        Some("v2")
    );
    let remaining: Vec<String> = store
        .checkpoint_records_page(&child.id, None, 1000)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(remaining.len(), 2);
    assert!(store.attachment(&child.id, &legacy_id).unwrap().is_none());
    assert_eq!(legacy_rows(&store, &child.id), 0);
    // Legacy blobs are not reclaimed at open; they go with the chat.
    assert_eq!(blob_rows(&store, &child.id, &plain), 1);
}

/// A chat marked by the v1 sweep still holds legacy records written after it,
/// or ones v1 kept. The v2 rule sweeps it again on the next open.
#[tokio::test]
async fn v1_marked_chat_is_swept_again_under_v2() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    let session = f.session.clone();
    let (legacy_id, _) = legacy_record(&f, &session, b"legacy preimage");
    {
        let path = f.store.chat_path(&session).unwrap().unwrap();
        rusqlite::Connection::open(path)
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO metadata VALUES('rewind_checkpoint_prune','v1')",
                [],
            )
            .unwrap();
    }
    let (_root, _data, store) = reopen(f);
    assert_eq!(
        store
            .chat_metadata(&session, "rewind_checkpoint_prune")
            .unwrap()
            .as_deref(),
        Some("v2")
    );
    assert!(store.attachment(&session, &legacy_id).unwrap().is_none());
    assert_eq!(legacy_rows(&store, &session), 0);
}

#[tokio::test]
async fn sweep_leaves_archived_chats_archived_and_writes_elsewhere_do_not_restore_them() {
    let f = Fixture::new();
    let archived = f.store.create_session("archived root").unwrap();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    let (legacy_id, _) = legacy_record(&f, &archived.id, b"archived preimage");
    assert!(f.store.archive_chat(&archived.id).unwrap());
    let data = f._data.path().join("chats");
    let files = |dir: &std::path::Path| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&archived.id))
            .collect();
        names.sort();
        names
    };
    let before = files(&data);
    assert!(
        !before
            .iter()
            .any(|name| name == &format!("{}.db", archived.id))
    );

    // Restart, then write in another chat. The archived chat must not be opened.
    let (_root, _data, store) = reopen(f);
    let other = store.create_session("other").unwrap().id;
    store.store_attachment(&other, "x", "{}", b"x").unwrap();
    assert_eq!(files(&data), before);
    assert!(
        store
            .list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.id == archived.id)
    );
    assert_eq!(files(&data), before);
    assert!(
        !files(&data)
            .iter()
            .any(|name| name == &format!("{}.db", archived.id))
    );
    let _ = legacy_id;
}

/// Provider for the sweep test, which never reaches a model call.
struct Silent;

#[async_trait]
impl Provider for Silent {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        anyhow::bail!("the sweep test makes no model call")
    }
}

/// The production host path sweeps a legacy chat on open. No install call is
/// made here: building the child host (profiles::child_agent_host, which goes
/// through AgentTools::new_recursive) must register the sweep itself.
#[tokio::test]
async fn production_host_sweeps_a_legacy_chat_on_open() {
    let f = Fixture::new();
    let (legacy_id, _) = legacy_record(&f, &f.session, b"legacy preimage");
    let Fixture {
        root,
        _data,
        store,
        harness,
        session,
    } = f;
    drop((store, harness));
    // Reopen without the sweep, so the chat is cached before any hook exists.
    let store = Store::open_data_dir(_data.path()).unwrap();
    assert_eq!(
        store
            .chat_metadata(&session, "rewind_checkpoint_prune")
            .unwrap(),
        None
    );

    let repl = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    let mut base = ToolRegistry::new();
    let processes = register_native_tools(&mut base, root.path(), false).unwrap();
    let _host = child_agent_host(
        store.clone(),
        Arc::new(Silent),
        &base,
        &repl,
        &HarnessConfig::default(),
        root.path(),
        false,
        None,
        Agents::default(),
        Arc::new(AgentRegistry::default()),
        processes,
    )
    .unwrap();

    assert_eq!(
        store
            .chat_metadata(&session, "rewind_checkpoint_prune")
            .unwrap()
            .as_deref(),
        Some("v2")
    );
    assert!(store.attachment(&session, &legacy_id).unwrap().is_none());
    assert_eq!(legacy_rows(&store, &session), 0);
}

/// Move a session's current-layout checkpoint records back to the layout an earlier
/// build wrote: attachment rows under the prefix whose blobs sit in `blobs`.
fn move_records_to_legacy_layout(f: &Fixture, session: &str) {
    let path = f.store.chat_path(session).unwrap().unwrap();
    let mut moved = Vec::new();
    for (id, metadata) in f.records(session) {
        let bytes = f.store.checkpoint_preimage(session, &id).unwrap().unwrap();
        moved.push((id, metadata, bytes));
    }
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("DELETE FROM checkpoint_records", []).unwrap();
    db.execute("DELETE FROM checkpoint_blobs", []).unwrap();
    // A chat from the earlier build has no upgrade marker; the sweep sets it.
    db.execute(
        "DELETE FROM metadata WHERE key='rewind_checkpoint_prune'",
        [],
    )
    .unwrap();
    for (id, metadata, bytes) in moved {
        let holder = format!("holder/{id}");
        let hash = f
            .store
            .store_attachment(session, &holder, "{}", &bytes)
            .unwrap();
        db.execute(
            "INSERT INTO attachments(session_id,id,metadata,blob_hash) VALUES(?1,?2,?3,?4)",
            rusqlite::params![session, id, metadata, hash],
        )
        .unwrap();
        db.execute(
            "DELETE FROM attachments WHERE session_id=?1 AND id=?2",
            rusqlite::params![session, holder],
        )
        .unwrap();
    }
}

/// A chat written by the main build keeps its latest turn rewindable after the
/// upgrade: the sweep copies the current and previous turn's legacy records.
#[tokio::test]
async fn main_format_chat_keeps_its_latest_turn_rewindable_after_upgrade() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    f.edit("a", "two", "three").await;
    let session = f.session.clone();
    let target = f.latest_target();
    move_records_to_legacy_layout(&f, &session);
    assert_eq!(legacy_rows(&f.store, &session), 2);

    let (root, _data, store) = reopen(f);
    assert_eq!(legacy_rows(&store, &session), 0);
    assert!(
        !store
            .checkpoint_records_page(&session, None, 10)
            .unwrap()
            .is_empty()
    );
    let result = rewind_session(&store, root.path(), &session, &target).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(fs::read_to_string(root.path().join("a")).unwrap(), "two\n");
}

/// One malformed legacy row must not stop the upgrade: the sweep leaves that row in
/// attachments and migrates the rest. Its turn is unknown, so the latest turn refuses
/// rather than rewinding without it.
#[tokio::test]
async fn malformed_legacy_row_is_left_in_place_and_the_latest_turn_refuses() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    f.edit("a", "two", "three").await;
    let session = f.session.clone();
    let target = f.latest_target();
    let oldest = f.records(&session).remove(0).0;
    move_records_to_legacy_layout(&f, &session);
    let path = f.store.chat_path(&session).unwrap().unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE attachments SET metadata='{not json' WHERE session_id=?1 AND id=?2",
            rusqlite::params![session, oldest],
        )
        .unwrap();
    assert_eq!(legacy_rows(&f.store, &session), 2);

    let (root, _data, store) = reopen(f);
    // The chat opened, so the marker is set and the malformed row stayed put.
    assert_eq!(
        store
            .chat_metadata(&session, "rewind_checkpoint_prune")
            .unwrap()
            .as_deref(),
        Some("v2")
    );
    assert_eq!(legacy_rows(&store, &session), 1);
    assert!(store.attachment(&session, &oldest).unwrap().is_some());
    let error = rewind_session(&store, root.path(), &session, &target).unwrap_err();
    assert!(
        format!("{error:#}")
            .starts_with("rewind refused: a legacy checkpoint with unreadable metadata"),
        "{error:#}"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("a")).unwrap(),
        "three\n"
    );
}

/// A turn holding a damaged row beside a valid one must not rewind to a partial
/// result. Both edits are one turn: rewinding it refuses, and nothing is written.
#[tokio::test]
async fn damaged_row_beside_a_valid_row_in_the_latest_turn_makes_rewind_refuse() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    let session = f.session.clone();
    let edit = |old: &str, new: &str| {
        format!(
            "await tools.edit({});",
            json!({"path":"a","old_string":old,"new_string":new})
        )
    };
    f.input(
        &session,
        json!({"code": format!("{}\nreturn {};", edit("one", "two"), edit("two", "three"))}),
    )
    .await;
    let target = f.latest_target();
    let damaged = f.records(&session).pop().unwrap().0;
    move_records_to_legacy_layout(&f, &session);
    let path = f.store.chat_path(&session).unwrap().unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE attachments SET metadata='{not json' WHERE session_id=?1 AND id=?2",
            rusqlite::params![session, damaged],
        )
        .unwrap();
    assert_eq!(legacy_rows(&f.store, &session), 2);

    let (root, _data, store) = reopen(f);
    assert_eq!(legacy_rows(&store, &session), 1);
    let sessions_before = store.chat_sessions(&session).unwrap().len();
    let error = rewind_session(&store, root.path(), &session, &target).unwrap_err();
    assert!(
        format!("{error:#}")
            .starts_with("rewind refused: a legacy checkpoint with unreadable metadata"),
        "{error:#}"
    );
    assert_eq!(
        store.chat_sessions(&session).unwrap().len(),
        sessions_before
    );
    assert_eq!(
        fs::read_to_string(root.path().join("a")).unwrap(),
        "three\n"
    );
}

/// A created-file row whose blob is missing migrates with an empty preimage written
/// through the checkpoint path. A later checkpoint of an existing empty file reuses
/// that preimage, and rewinding it restores the empty file.
#[tokio::test]
async fn empty_placeholder_preimage_restores_a_later_empty_file_checkpoint() {
    let f = Fixture::new();
    fs::write(f.root.path().join("e.txt"), "").unwrap();
    f.write("new.txt", "hello\n").await;
    let session = f.session.clone();
    move_records_to_legacy_layout(&f, &session);
    drop_legacy_blob(&f, &session);
    // Reopening migrates the chat, then a new turn writes an existing empty file.
    let f = reopened(f);
    assert_eq!(legacy_rows(&f.store, &session), 0);

    f.write("e.txt", "filled\n").await;
    let target = f.latest_target();
    let result = rewind_session(&f.store, f.root.path(), &session, &target).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert!(result.restored.iter().any(|path| path == "e.txt"));
    assert_eq!(fs::read(f.root.path().join("e.txt")).unwrap(), b"");
}

/// An unrestorable record has no preimage. Export writes it with a null
/// `preimage_hash` and no data, and does not fail.
#[tokio::test]
async fn export_writes_an_unrestorable_record_without_a_preimage() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    let session = f.session.clone();
    move_records_to_legacy_layout(&f, &session);
    drop_legacy_blob(&f, &session);
    let f = reopened(f);

    let mut bytes = Vec::new();
    f.store.export_jsonl(&session, &mut bytes).unwrap();
    let entries: Vec<Value> = String::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap()
                .get("checkpoints")
                .cloned()
        })
        .flat_map(|checkpoints| checkpoints.as_array().cloned().unwrap_or_default())
        .collect();
    let unrestorable: Vec<&Value> = entries
        .iter()
        .filter(|entry| !entry["metadata"]["unrestorable"].is_null())
        .collect();
    assert_eq!(unrestorable.len(), 1, "{entries:?}");
    assert_eq!(unrestorable[0]["preimage_hash"], Value::Null);
    assert!(unrestorable[0].get("data").is_none());
}

/// Remove one legacy row's blob from a chat file. Foreign keys are off on this
/// connection only, so the row keeps pointing at a missing blob.
fn drop_legacy_blob(f: &Fixture, session: &str) {
    let path = f.store.chat_path(session).unwrap().unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let hash: String = db
        .query_row(
            "SELECT blob_hash FROM attachments WHERE session_id=?1",
            [session],
            |row| row.get(0),
        )
        .unwrap();
    db.execute("DELETE FROM blobs WHERE hash=?1", [hash])
        .unwrap();
}

/// A created file has no preimage, so a missing legacy blob does not stop the
/// upgrade: rewind still deletes the file.
#[tokio::test]
async fn created_file_with_missing_blob_still_rewinds_by_deletion() {
    let f = Fixture::new();
    f.write("new.txt", "hello\n").await;
    let session = f.session.clone();
    let target = f.latest_target();
    move_records_to_legacy_layout(&f, &session);
    drop_legacy_blob(&f, &session);

    let (root, _data, store) = reopen(f);
    assert_eq!(legacy_rows(&store, &session), 0);
    let result = rewind_session(&store, root.path(), &session, &target).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert!(result.deleted.iter().any(|path| path == "new.txt"));
    assert!(!root.path().join("new.txt").exists());
}

/// A kept-turn row whose preimage is gone is migrated as unrestorable. Rewinding
/// that turn refuses before forking or writing anything.
#[tokio::test]
async fn unrestorable_record_in_the_kept_turn_makes_rewind_refuse() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    let session = f.session.clone();
    let target = f.latest_target();
    move_records_to_legacy_layout(&f, &session);
    drop_legacy_blob(&f, &session);

    let (root, _data, store) = reopen(f);
    let sessions_before = store.chat_sessions(&session).unwrap().len();
    let error = rewind_session(&store, root.path(), &session, &target).unwrap_err();
    assert!(
        format!("{error:#}").starts_with("rewind refused: a has an unrestorable checkpoint"),
        "{error:#}"
    );
    assert_eq!(
        store.chat_sessions(&session).unwrap().len(),
        sessions_before
    );
    assert_eq!(fs::read_to_string(root.path().join("a")).unwrap(), "two\n");
}

/// A chat whose upgrade has not run refuses rewind before it forks anything.
#[tokio::test]
async fn rewind_refuses_a_chat_whose_checkpoint_upgrade_has_not_run() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    let session = f.session.clone();
    let target = f.latest_target();
    let path = f.store.chat_path(&session).unwrap().unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "DELETE FROM metadata WHERE key='rewind_checkpoint_prune'",
            [],
        )
        .unwrap();
    let sessions_before = f.store.chat_sessions(&session).unwrap().len();

    let error = rewind_session(&f.store, f.root.path(), &session, &target).unwrap_err();
    assert!(
        format!("{error:#}").contains("upgrade has not run"),
        "{error:#}"
    );
    assert_eq!(
        f.store.chat_sessions(&session).unwrap().len(),
        sessions_before
    );
    assert_eq!(
        fs::read_to_string(f.root.path().join("a")).unwrap(),
        "two\n"
    );
}

#[path = "rewind/checkpoint_export.rs"]
mod checkpoint_export;
