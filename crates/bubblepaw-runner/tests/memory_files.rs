//! Scripted memory-file acceptance; no model or external transport calls.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    Store, TextCallback, ToolRegistry,
};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions, Override},
    memory_files::{MAX_MEMORY_BYTES, MemoryFiles},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[path = "support/notice_fixture.rs"]
mod notice_fixture;
use notice_fixture::Notices;

struct Done;
#[async_trait]
impl Provider for Done {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}
fn write(path: impl AsRef<Path>, text: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
fn memory(store: &Store, workspace: &Path, user: Option<PathBuf>) -> MemoryFiles {
    MemoryFiles::new(store.clone(), workspace.to_owned(), user, true)
}
fn persist_notices(store: &Store, memory: &MemoryFiles, session: &str) -> Vec<String> {
    let notices = memory.take_changes(session);
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(Notices::new("memory_notices", notices.clone())))
        .unwrap();
    let harness = Harness::new(
        store.clone(),
        Arc::new(Done),
        tools,
        HarnessConfig::default(),
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let run = runtime
        .block_on(harness.prompt(session, "persist fixture notice", RunOptions::default()))
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    notices
}

#[test]
fn git_order_both_names_identical_dedup_and_no_import_expansion() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let workspace = repo.join("one/two");
    std::fs::create_dir_all(&workspace).unwrap();
    write(repo.join(".git"), "gitdir: elsewhere");
    write(root.path().join("AGENTS.md"), "outside git root");
    let user = root.path().join("user");
    write(user.join("AGENTS.md"), "USER-FIRST");
    write(user.join("CLAUDE.md"), "user CLAUDE excluded");
    write(repo.join("AGENTS.md"), "ROOT-AGENTS");
    write(repo.join("CLAUDE.md"), "ROOT-CLAUDE");
    write(repo.join("one/AGENTS.md"), "MIDDLE");
    write(repo.join("one/CLAUDE.md"), "MIDDLE");
    write(workspace.join("AGENTS.md"), "LEAF-AGENTS @import.md");
    write(workspace.join("CLAUDE.md"), "LEAF-CLAUDE");
    write(workspace.join("import.md"), "do not expand imports");
    let store = Store::in_memory().unwrap();
    let session = store.create_session("order").unwrap();
    let memory = memory(&store, &workspace, Some(user));
    let prompt = memory.system_prompt(&session.id, "base").unwrap();
    let mut position = 0;
    for text in [
        "USER-FIRST",
        "ROOT-AGENTS",
        "ROOT-CLAUDE",
        "MIDDLE",
        "LEAF-AGENTS",
        "LEAF-CLAUDE",
    ] {
        let next = prompt.find(text).unwrap();
        assert!(next > position, "{text}: {prompt}");
        position = next;
    }
    assert_eq!(prompt.matches("MIDDLE").count(), 1);
    assert!(prompt.contains("@import.md"));
    for excluded in [
        "outside git root",
        "user CLAUDE excluded",
        "do not expand imports",
    ] {
        assert!(!prompt.contains(excluded));
    }
    assert!(memory.take_changes(&session.id).is_empty());
}

#[test]
fn no_git_limits_discovery_to_workspace_and_off_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    write(root.path().join("AGENTS.md"), "PARENT-EXCLUDED");
    write(workspace.join("AGENTS.md"), "LOCAL-AGENTS");
    write(workspace.join("CLAUDE.md"), "LOCAL-CLAUDE");
    let store = Store::in_memory().unwrap();
    let session = store.create_session("local").unwrap();
    let off = MemoryFiles::new(store.clone(), workspace.clone(), None, false);
    let before = store.storage_stats().unwrap();
    assert_eq!(off.system_prompt(&session.id, "base").unwrap(), "base");
    off.check(&session.id).unwrap();
    assert!(off.take_changes(&session.id).is_empty());
    assert_eq!(
        store.storage_stats().unwrap().transaction_count,
        before.transaction_count
    );
    let memory = memory(&store, &workspace, None);
    let prompt = memory.system_prompt(&session.id, "base").unwrap();
    assert!(prompt.contains("LOCAL-AGENTS") && prompt.contains("LOCAL-CLAUDE"));
    assert!(!prompt.contains("PARENT-EXCLUDED"));
}

#[test]
fn utf8_total_cap_and_changed_file_has_fresh_budget_after_full_prefix() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path().join("AGENTS.md"),
        &"猫".repeat(MAX_MEMORY_BYTES),
    );
    write(root.path().join("CLAUDE.md"), "initial hidden tail");
    let store = Store::in_memory().unwrap();
    let session = store.create_session("cap").unwrap();
    let memory = memory(&store, root.path(), None);
    let frozen = memory.system_prompt(&session.id, "").unwrap();
    assert!(frozen.len() <= MAX_MEMORY_BYTES + 2);
    assert!(frozen.contains("truncated at 32 KiB"));
    assert!(!frozen.contains("initial hidden tail"));
    write(
        root.path().join("CLAUDE.md"),
        "CHANGED-TAIL-GETS-OWN-BUDGET",
    );
    memory.check(&session.id).unwrap();
    let notices = memory.take_changes(&session.id);
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("CHANGED-TAIL-GETS-OWN-BUDGET"));
    assert!(notices[0].len() <= MAX_MEMORY_BYTES);
    assert_eq!(memory.system_prompt(&session.id, "").unwrap(), frozen);
    // Same-size edits beyond the frozen text cap are still noticed.
    write(
        root.path().join("AGENTS.md"),
        &format!("{}犬", "猫".repeat(MAX_MEMORY_BYTES - 1)),
    );
    memory.check(&session.id).unwrap();
    let notices = memory.take_changes(&session.id);
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("AGENTS.md"));
    assert!(notices[0].contains("\"truncated\":true"));
    assert!(notices[0].len() <= MAX_MEMORY_BYTES);
}

#[test]
fn oversized_identical_files_are_deduplicated_and_point_to_the_first_path() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let workspace = root.path().join("nested");
    let large = "same oversized instructions\n".repeat(MAX_MEMORY_BYTES / 10);
    write(root.path().join("AGENTS.md"), &large);
    write(root.path().join("CLAUDE.md"), &large);
    write(workspace.join("AGENTS.md"), &large);
    let store = Store::in_memory().unwrap();
    let session = store.create_session("large duplicate").unwrap();
    let instructions = memory(&store, &workspace, None);
    let prompt = instructions.system_prompt(&session.id, "").unwrap();
    assert!(prompt.contains("truncated at 32 KiB"));
    let bytes = store
        .attachment(&session.id, "bubblepaw-memory-files-v1")
        .unwrap()
        .unwrap()
        .1;
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let files = snapshot["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(files[1]["duplicate_of"], files[0]["path"]);
    assert_eq!(files[2]["duplicate_of"], files[0]["path"]);
    assert_eq!(files[1]["content"], "");
    assert_eq!(files[2]["content"], "");
}

#[test]
fn changes_add_remove_once_reopen_freeze_and_unchanged_zero_writes() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let path = workspace.join("AGENTS.md");
    write(&path, "ORIGINAL");
    let db = root.path().join("chat.db");
    let store = Store::open(&db).unwrap();
    let session = store.create_session("durable").unwrap();
    let instructions = memory(&store, &workspace, None);
    let frozen = instructions.system_prompt(&session.id, "base").unwrap();
    let before = store.storage_stats().unwrap();
    for _ in 0..12 {
        instructions.check(&session.id).unwrap();
        assert_eq!(
            instructions.system_prompt(&session.id, "base").unwrap(),
            frozen
        );
        assert!(instructions.take_changes(&session.id).is_empty());
    }
    let after = store.storage_stats().unwrap();
    assert_eq!(before.transaction_count, after.transaction_count);
    assert_eq!(before.wal_bytes, after.wal_bytes);
    write(&path, "EDITED-CONTENT");
    instructions.check(&session.id).unwrap();
    let notice = persist_notices(&store, &instructions, &session.id);
    assert_eq!(notice.len(), 1);
    assert!(notice[0].contains("EDITED-CONTENT"));
    // A same-content rewrite may change metadata but is not a content change.
    write(&path, "EDITED-CONTENT");
    instructions.check(&session.id).unwrap();
    assert!(instructions.take_changes(&session.id).is_empty());
    drop(instructions);
    drop(store);
    let store = Store::open(&db).unwrap();
    let instructions = memory(&store, &workspace, None);
    let before = store.storage_stats().unwrap();
    assert_eq!(
        instructions.system_prompt(&session.id, "base").unwrap(),
        frozen
    );
    assert!(instructions.take_changes(&session.id).is_empty());
    assert_eq!(
        store.storage_stats().unwrap().transaction_count,
        before.transaction_count
    );
    write(workspace.join("CLAUDE.md"), "ADDED");
    instructions.check(&session.id).unwrap();
    let added = persist_notices(&store, &instructions, &session.id);
    assert_eq!(added.len(), 1);
    assert!(added[0].contains("CLAUDE.md") && added[0].contains("ADDED"));
    std::fs::remove_file(&path).unwrap();
    instructions.check(&session.id).unwrap();
    let removed = persist_notices(&store, &instructions, &session.id);
    assert_eq!(removed.len(), 1);
    assert!(removed[0].contains("AGENTS.md") && removed[0].contains("\"content\":null"));
    instructions.check(&session.id).unwrap();
    assert!(instructions.take_changes(&session.id).is_empty());
    assert_eq!(
        instructions.system_prompt(&session.id, "base").unwrap(),
        frozen
    );
    drop(instructions);
    let instructions = memory(&store, &workspace, None);
    instructions.check(&session.id).unwrap();
    assert!(
        instructions.take_changes(&session.id).is_empty(),
        "removal remains acknowledged after reopen"
    );
}

#[test]
fn child_sessions_freeze_their_own_workspace_and_start_time() {
    let root = tempfile::tempdir().unwrap();
    let parent_dir = root.path().join("parent");
    let child_dir = root.path().join("child");
    write(parent_dir.join("AGENTS.md"), "PARENT-CONTENT");
    write(child_dir.join("AGENTS.md"), "CHILD-CONTENT");
    let store = Store::in_memory().unwrap();
    let parent = store.create_session("parent").unwrap();
    let child = store
        .create_session_with_parent("child", Default::default(), Some(&parent.id))
        .unwrap();
    let parent_memory = memory(&store, &parent_dir, None);
    let child_memory = memory(&store, &child_dir, None);
    let parent_prompt = parent_memory
        .system_prompt(&parent.id, "parent base")
        .unwrap();
    let child_prompt = child_memory.system_prompt(&child.id, "child base").unwrap();
    assert!(parent_prompt.contains("PARENT-CONTENT") && !parent_prompt.contains("CHILD-CONTENT"));
    assert!(child_prompt.contains("CHILD-CONTENT") && !child_prompt.contains("PARENT-CONTENT"));
    write(child_dir.join("AGENTS.md"), "NEW-CHILD-CONTENT");
    child_memory.check(&child.id).unwrap();
    assert_eq!(child_memory.take_changes(&child.id).len(), 1);
    assert!(parent_memory.take_changes(&parent.id).is_empty());
    assert_eq!(
        child_memory.system_prompt(&child.id, "child base").unwrap(),
        child_prompt
    );
}

#[test]
fn several_changes_in_one_durable_notice_batch_restore_without_redelivery() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("AGENTS.md");
    write(&path, "version one");
    let store = Store::in_memory().unwrap();
    let session = store.create_session("batched notice").unwrap();
    let instructions = memory(&store, root.path(), None);
    let frozen = instructions.system_prompt(&session.id, "base").unwrap();
    write(&path, "version two");
    instructions.check(&session.id).unwrap();
    write(&path, "version three");
    instructions.check(&session.id).unwrap();
    assert_eq!(persist_notices(&store, &instructions, &session.id).len(), 2);
    drop(instructions);
    let instructions = memory(&store, root.path(), None);
    assert_eq!(
        instructions.system_prompt(&session.id, "base").unwrap(),
        frozen
    );
    assert!(instructions.take_changes(&session.id).is_empty());
}

#[test]
fn ten_kib_snapshot_storage_measurement() {
    let root = tempfile::tempdir().unwrap();
    write(root.path().join("AGENTS.md"), &"x".repeat(10 * 1024));
    let store = Store::open_data_dir(root.path().join("data")).unwrap();
    let session = store.create_session("measurement").unwrap();
    store.idle_checkpoint().unwrap();
    let before = store.storage_stats().unwrap();
    let memory = memory(&store, root.path(), None);
    memory.check(&session.id).unwrap();
    let after = store.storage_stats().unwrap();
    assert_eq!(after.transaction_count - before.transaction_count, 1);
    assert_eq!(after.message_count, before.message_count);
    let bytes = store
        .attachment(&session.id, "bubblepaw-memory-files-v1")
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(
        bytes
            .windows(10 * 1024)
            .filter(|window| window.iter().all(|byte| *byte == b'x'))
            .count(),
        1
    );
    for _ in 0..20 {
        memory.check(&session.id).unwrap();
    }
    let idle = store.storage_stats().unwrap();
    assert_eq!(idle.transaction_count, after.transaction_count);
    assert_eq!(idle.wal_bytes, after.wal_bytes);
    println!(
        "memory 10KiB: attachment={} bytes, page_growth={} bytes, database_growth={} bytes, chat_growth={} bytes, wal_growth={} bytes, transactions={}, chat_transactions={}, unchanged_transactions=0, unchanged_wal_growth=0",
        bytes.len(),
        after.page_bytes - before.page_bytes,
        after.database_bytes - before.database_bytes,
        after.chat_bytes - before.chat_bytes,
        after.wal_bytes - before.wal_bytes,
        after.transaction_count - before.transaction_count,
        after.chat_transaction_count - before.chat_transaction_count
    );
}

#[test]
fn memory_files_switch_defaults_on_and_is_restart_only() {
    let root = tempfile::tempdir().unwrap();
    let options = LoadOptions {
        workspace: root.path().to_owned(),
        home: None,
        custom_file: None,
        env: vec![],
        cli: vec![],
    };
    let startup = EffectiveConfig::load(options.clone()).unwrap();
    assert!(startup.settings().unwrap().memory_files);
    let mut disabled = options;
    disabled
        .cli
        .push(Override::set("context.memory_files=false").unwrap());
    let disabled = EffectiveConfig::load(disabled).unwrap();
    assert!(!disabled.settings().unwrap().memory_files);
    assert!(disabled.restart_needed(&startup));
}
