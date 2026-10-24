use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, Role, RunOptions, Store, ToolCall, ToolContext,
    ToolRegistry,
};
use bubblepaw_runner::{
    provider::{ScriptedProvider, ScriptedStep},
    rewind::rewind_session,
    tools::WorkspaceTools,
};
use serde_json::{Value, json};
use std::{fs, sync::Arc};

struct Fixture {
    root: tempfile::TempDir,
    store: Store,
    session: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("literal file tools").unwrap().id;
        Self {
            root,
            store,
            session,
        }
    }

    fn registry(&self, read_only: bool) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        WorkspaceTools::new(self.root.path())
            .unwrap()
            .register(&mut registry, read_only)
            .unwrap();
        registry
    }

    async fn call(&self, name: &str, input: Value) -> Result<Value> {
        // Production keeps file tools host-only under js. This fixture exposes
        // one existing capability so the real dispatcher supplies its Store
        // and durable effect identity, just as it does for nested js calls.
        let mut tools = ToolRegistry::new();
        tools.register(
            self.registry(false)
                .get(name)
                .context("missing file tool")?,
        )?;
        let call_id = uuid::Uuid::new_v4().to_string();
        let provider = ScriptedProvider::with_steps(
            vec![ScriptedStep::Calls {
                calls: vec![ToolCall {
                    id: call_id.clone(),
                    name: name.into(),
                    arguments: input,
                }],
            }],
            "done",
        );
        let harness = Harness::new(
            self.store.clone(),
            Arc::new(provider),
            tools,
            HarnessConfig::default(),
        );
        let run = harness
            .prompt(
                &self.session,
                "apply fixture operation",
                RunOptions::default(),
            )
            .await?;
        ensure!(run.status == "completed", "run failed: {:?}", run.error);
        let result = self
            .store
            .messages(&self.session)?
            .into_iter()
            .find(|message| {
                message.role == Role::Tool
                    && message.tool_call_id.as_deref() == Some(call_id.as_str())
            })
            .context("missing file tool result")?;
        if result.is_error {
            bail!(result.content);
        }
        Ok(serde_json::from_str(&result.content)?)
    }

    fn checkpoints(&self) -> Vec<(String, String)> {
        self.store
            .checkpoint_records_page(&self.session, None, 100)
            .unwrap()
    }
}

#[tokio::test]
async fn unique_literal_edit_preserves_surrounding_bytes_and_returns_only_contract_fields() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a.txt"), "head\n  old\ntail").unwrap();
    let result = f
        .call(
            "edit",
            json!({"path":"a.txt","old_string":"  old","new_string":"  new"}),
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"path":"a.txt","replacements":1}));
    assert_eq!(
        fs::read(f.root.path().join("a.txt")).unwrap(),
        b"head\n  new\ntail"
    );
    assert_eq!(f.checkpoints().len(), 1);
}

#[tokio::test]
async fn no_match_reports_nearest_numbered_region_and_does_not_write_or_checkpoint() {
    let f = Fixture::new();
    let original = "unrelated\n  let count = 42;\ntrailer\n";
    fs::write(f.root.path().join("a.rs"), original).unwrap();
    let error = f
        .call(
            "edit",
            json!({"path":"a.rs","old_string":"  let count = 41;","new_string":"replacement"}),
        )
        .await
        .unwrap_err()
        .to_string();
    for expected in [
        "a.rs",
        "no exact match",
        "Nearest region",
        "2:   let count = 42;",
        "First difference",
        "fresh read",
    ] {
        assert!(error.contains(expected), "missing {expected:?} in {error}");
    }
    assert_eq!(
        fs::read_to_string(f.root.path().join("a.rs")).unwrap(),
        original
    );
    assert!(f.checkpoints().is_empty());
}

#[tokio::test]
async fn ambiguity_reports_count_and_lines_then_replace_all_changes_every_match() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a.txt"), "cat\nkeep\ncat\ncat").unwrap();
    let error = f
        .call(
            "edit",
            json!({"path":"a.txt","old_string":"cat","new_string":"dog"}),
        )
        .await
        .unwrap_err()
        .to_string();
    for expected in [
        "3 occurrences",
        "1, 3, 4",
        "surrounding context",
        "replace_all",
    ] {
        assert!(error.contains(expected), "{error}");
    }
    assert!(f.checkpoints().is_empty());
    let result = f
        .call(
            "edit",
            json!({"path":"a.txt","old_string":"cat","new_string":"dog","replace_all":true}),
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"path":"a.txt","replacements":3}));
    assert_eq!(
        fs::read_to_string(f.root.path().join("a.txt")).unwrap(),
        "dog\nkeep\ndog\ndog"
    );
}

#[tokio::test]
async fn invalid_edits_and_missing_files_are_actionable_and_have_no_effect() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a.txt"), "original").unwrap();
    for (input, expected) in [
        (
            json!({"path":"a.txt","old_string":"original","new_string":"original"}),
            "must differ",
        ),
        (
            json!({"path":"a.txt","old_string":"","new_string":"new"}),
            "must not be empty",
        ),
        (
            json!({"path":"missing.txt","old_string":"old","new_string":"new"}),
            "use write to create it",
        ),
    ] {
        let path = input["path"].as_str().unwrap().to_owned();
        let error = f.call("edit", input).await.unwrap_err().to_string();
        assert!(error.contains(expected) && error.contains(&path), "{error}");
    }
    let schema_error = f
        .call(
            "edit",
            json!({"path":"a.txt","old_string":"original","new_string":"new","replace_all":"true"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(schema_error.contains("boolean"), "{schema_error}");
    assert_eq!(fs::read(f.root.path().join("a.txt")).unwrap(), b"original");
    assert!(!f.root.path().join("missing.txt").exists());
    assert!(f.checkpoints().is_empty());
}

#[tokio::test]
async fn write_creates_parents_then_overwrites_with_exact_utf8_byte_count() {
    let f = Fixture::new();
    let content = "hé 🐱\r\n`$value`";
    let result = f
        .call(
            "write",
            json!({"path":"nested/deep/a.txt","content":content}),
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"path":"nested/deep/a.txt","bytes":content.len(),"created":true})
    );
    assert_eq!(
        fs::read(f.root.path().join("nested/deep/a.txt")).unwrap(),
        content.as_bytes()
    );
    let result = f
        .call("write", json!({"path":"nested/deep/a.txt","content":""}))
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"path":"nested/deep/a.txt","bytes":0,"created":false})
    );
    assert!(
        fs::read(f.root.path().join("nested/deep/a.txt"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.checkpoints().len(), 2);
}

#[tokio::test]
async fn crlf_unicode_backticks_and_dollars_remain_literal() {
    let f = Fixture::new();
    let original = "head\r\n  hé 🐱 `$value`\r\ntail";
    fs::write(f.root.path().join("a.txt"), original).unwrap();
    // LF does not match CRLF, and a decomposed accent does not match composed é.
    for old in ["  hé 🐱 `$value`\n", "  he\u{301} 🐱 `$value`\r\n"] {
        let error = f
            .call(
                "edit",
                json!({"path":"a.txt","old_string":old,"new_string":"wrong"}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no exact match"), "{error}");
    }
    let result = f.call("edit", json!({"path":"a.txt","old_string":"  hé 🐱 `$value`\r\n","new_string":"  hé 🐶 `${other}`\r\n"})).await.unwrap();
    assert_eq!(result["replacements"], 1);
    assert_eq!(
        fs::read(f.root.path().join("a.txt")).unwrap(),
        "head\r\n  hé 🐶 `${other}`\r\ntail".as_bytes()
    );
}

#[tokio::test]
async fn writes_and_edits_cannot_escape_the_workspace() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("outside.txt");
    fs::write(&target, "old").unwrap();
    for (name, input) in [
        ("write", json!({"path":target,"content":"new"})),
        (
            "edit",
            json!({"path":target,"old_string":"old","new_string":"new"}),
        ),
        ("write", json!({"path":"../escape.txt","content":"new"})),
    ] {
        let error = f.call(name, input).await.unwrap_err().to_string();
        assert!(
            error.contains("workspace-relative") || error.contains("escapes workspace"),
            "{error}"
        );
    }
    assert_eq!(fs::read(target).unwrap(), b"old");
    assert!(f.checkpoints().is_empty());
}

#[tokio::test]
async fn read_only_registration_omits_mutators_and_direct_invocation_cannot_bypass_checkpoints() {
    let f = Fixture::new();
    let read_only = f.registry(true);
    assert!(read_only.get("read").is_some());
    for name in ["edit", "write"] {
        assert!(read_only.get(name).is_none());
        let tool = f.registry(false).get(name).unwrap();
        assert!(!tool.definition().read_only);
        let error = tool
            .invoke(
                ToolContext {
                    session_id: f.session.clone(),
                    run_id: "direct-run".into(),
                    invocation_id: "direct-call".into(),
                    cancellation: CancellationToken::new(),
                    approval: Default::default(),
                },
                json!({"path":"a.txt","content":"unsafe","old_string":"a","new_string":"b"}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("dispatcher"), "{error}");
    }
    assert_eq!(
        f.registry(false)
            .capabilities()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["edit", "list", "read", "search", "write"]
    );
    assert!(f.registry(false).definitions().is_empty());
    assert!(!f.root.path().join("a.txt").exists());
}

/// An in-memory store has no legacy layout and never runs the sweep, so it rewinds
/// its edits without waiting for an upgrade marker.
#[tokio::test]
async fn in_memory_store_records_an_edit_and_rewinds_it() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one").unwrap();
    f.call(
        "edit",
        json!({"path":"a","old_string":"one","new_string":"two"}),
    )
    .await
    .unwrap();
    let latest = f
        .store
        .messages(&f.session)
        .unwrap()
        .into_iter()
        .rfind(bubblepaw_core::is_typed_user)
        .unwrap()
        .id;
    let result = rewind_session(&f.store, f.root.path(), &f.session, &latest).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(result.restored, ["a"]);
    assert_eq!(fs::read(f.root.path().join("a")).unwrap(), b"one");
}

#[tokio::test]
async fn edit_and_write_share_durable_rewind_history() {
    let f = Fixture::new();
    fs::write(f.root.path().join("original.txt"), "before").unwrap();
    f.call(
        "edit",
        json!({"path":"original.txt","old_string":"before","new_string":"after"}),
    )
    .await
    .unwrap();
    f.call("write", json!({"path":"new.txt","content":"created"}))
        .await
        .unwrap();
    f.call(
        "write",
        json!({"path":"original.txt","content":"overwritten"}),
    )
    .await
    .unwrap();
    // The third turn's write pruned the first turn's record: checkpoints are
    // kept for the current and previous turn only.
    assert_eq!(f.checkpoints().len(), 2);
    // Each call is its own turn. Only the latest user message is a rewind
    // target, so the first turn's message is rejected and nothing is restored.
    let first_turn = f.store.messages(&f.session).unwrap()[0].id.clone();
    let error = rewind_session(&f.store, f.root.path(), &f.session, &first_turn)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("only the last message can be rewound"),
        "{error}"
    );
    assert_eq!(
        fs::read(f.root.path().join("original.txt")).unwrap(),
        b"overwritten"
    );
    assert!(f.root.path().join("new.txt").exists());

    // Rewinding the latest turn undoes only its own overwrite. The earlier
    // created file and edit stay in place.
    let latest = f
        .store
        .messages(&f.session)
        .unwrap()
        .into_iter()
        .rfind(bubblepaw_core::is_typed_user)
        .unwrap()
        .id;
    let result = rewind_session(&f.store, f.root.path(), &f.session, &latest).unwrap();
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert_eq!(result.restored, ["original.txt"]);
    assert!(result.deleted.is_empty());
    assert_eq!(
        fs::read(f.root.path().join("original.txt")).unwrap(),
        b"after"
    );
    assert_eq!(fs::read(f.root.path().join("new.txt")).unwrap(), b"created");
}

#[tokio::test]
async fn unreadable_checkpoint_metadata_aborts_before_mutating_files() {
    let f = Fixture::new();
    fs::write(f.root.path().join("a.txt"), "old").unwrap();
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
    for (name, input) in [
        (
            "edit",
            json!({"path":"a.txt","old_string":"old","new_string":"new"}),
        ),
        ("write", json!({"path":"nested/new.txt","content":"new"})),
    ] {
        let error = f.call(name, input).await.unwrap_err().to_string();
        assert!(
            error.contains("invalid file checkpoint metadata"),
            "{error}"
        );
    }
    assert_eq!(fs::read(f.root.path().join("a.txt")).unwrap(), b"old");
    assert!(!f.root.path().join("nested").exists());
}

#[tokio::test]
async fn diagnostics_and_replacement_growth_are_bounded_before_writing() {
    let f = Fixture::new();
    let long_line = "x".repeat(1024 * 1024);
    fs::write(f.root.path().join("long.txt"), &long_line).unwrap();
    let error = f
        .call(
            "edit",
            json!({"path":"long.txt","old_string":"z","new_string":"new"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Nearest region") && error.len() < 2000,
        "{} bytes",
        error.len()
    );
    assert_eq!(
        fs::read_to_string(f.root.path().join("long.txt")).unwrap(),
        long_line
    );
    let repeated = "a".repeat(4096);
    fs::write(f.root.path().join("grow.txt"), &repeated).unwrap();
    let error = f.call("edit", json!({"path":"grow.txt","old_string":"a","new_string":"b".repeat(8193),"replace_all":true})).await.unwrap_err().to_string();
    assert!(error.contains("32 MiB"), "{error}");
    assert_eq!(
        fs::read_to_string(f.root.path().join("grow.txt")).unwrap(),
        repeated
    );
    assert!(f.checkpoints().is_empty());
}

#[tokio::test]
async fn nearest_region_scans_late_file_lines_and_absolute_inside_paths_are_rejected() {
    let f = Fixture::new();
    let source = format!("{}  let selected = 42;\nend\n", "unrelated\n".repeat(3000));
    fs::write(f.root.path().join("late.rs"), &source).unwrap();
    let error = f
        .call(
            "edit",
            json!({"path":"late.rs","old_string":"  let selected = 41;","new_string":"changed"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("3001:   let selected = 42;"), "{error}");
    let absolute = f.root.path().join("late.rs");
    for (name, input) in [
        (
            "edit",
            json!({"path":absolute,"old_string":"selected","new_string":"changed"}),
        ),
        ("write", json!({"path":absolute,"content":"changed"})),
    ] {
        let error = f.call(name, input).await.unwrap_err().to_string();
        assert!(error.contains("workspace-relative"), "{error}");
    }
    assert_eq!(
        fs::read_to_string(f.root.path().join("late.rs")).unwrap(),
        source
    );
}
