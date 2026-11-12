use bubblepaw_core::transcript::{normalized_derived, normalized_message};
use bubblepaw_core::{
    ImportFormat, ImportOptions, Message, Role, Store, StoreOptions, import_file,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::Command,
};

fn write_source(path: &Path, events: &[Value]) {
    let mut file = File::create(path).unwrap();
    for event in events {
        serde_json::to_writer(&mut file, event).unwrap();
        file.write_all(b"\n").unwrap();
    }
}

fn response(payload: Value) -> Value {
    json!({"type":"response_item","timestamp":"2026-10-12T00:00:00Z","payload":payload})
}

/// Interpreter candidates as program plus leading arguments. `python3` is
/// Linux's name; Windows may provide `python` or the `py -3` launcher instead.
const PYTHON3_CANDIDATES: &[(&str, &[&str])] =
    &[("python3", &[]), ("python", &[]), ("py", &["-3"])];

/// Returns the first candidate that actually runs Python 3. A Windows Store
/// execution alias without an installed Python fails this probe, so it is
/// never mistaken for a working interpreter.
fn working_python3() -> Result<(&'static str, &'static [&'static str]), String> {
    let mut failures = Vec::new();
    for &(program, args) in PYTHON3_CANDIDATES {
        let probe = Command::new(program)
            .args(args)
            .args([
                "-c",
                "import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)",
            ])
            .output();
        match probe {
            Ok(output) if output.status.success() => return Ok((program, args)),
            Ok(output) => failures.push(format!("{program}: exit {}", output.status)),
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }
    Err(failures.join("; "))
}

#[test]
fn committed_generator_covers_multiple_compactions_and_blob_backed_roundtrips() {
    const TURNS: u64 = 256;
    const PAYLOAD_BYTES: i64 = 8192;
    let (program, args) = match working_python3() {
        Ok(found) => found,
        Err(reasons) => {
            // Direct stderr stays visible under libtest's normal output capture.
            let _ = writeln!(
                std::io::stderr().lock(),
                "SKIP committed synthetic generator: no working Python 3 interpreter ({reasons})"
            );
            return;
        }
    };
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    let generator =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generate_synthetic.py");
    // A working interpreter was found, so any failure from here is a real
    // generator failure and must fail the test.
    let output = Command::new(program)
        .args(args)
        .arg(&generator)
        .arg("--out")
        .arg(&sources)
        .arg("--turns")
        .arg(TURNS.to_string())
        .arg("--payload-bytes")
        .arg(PAYLOAD_BYTES.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Committed synthetic generator failed under {program}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(generated["turns"], TURNS);
    assert_eq!(generated["payload_bytes"], PAYLOAD_BYTES);
    assert_eq!(generated["files"], 4);

    let total_turns = TURNS + 1;
    let compactions = TURNS.div_ceil(4) + 1;
    for (
        filename,
        format,
        messages_per_turn,
        calls_per_turn,
        derived_per_turn,
        reasoning_per_turn,
        excluded_kind,
    ) in [
        (
            "codex.jsonl",
            ImportFormat::Codex,
            8,
            2,
            0,
            1,
            "harness_role",
        ),
        (
            "claude.jsonl",
            ImportFormat::Claude,
            5,
            1,
            1,
            2,
            "harness_user",
        ),
    ] {
        // Inspect large-content storage before the explicit default-policy row
        // compression pass. Import's idle housekeeping must not run that pass first.
        let store = Store::open_with_options(
            temp.path().join(format!("store-{filename}")),
            StoreOptions {
                archive_after_days: 0,
                cold_min_messages: u32::MAX,
                cold_min_bytes: u64::MAX,
                ..StoreOptions::default()
            },
        )
        .unwrap();
        let imported = import_file(
            &store,
            sources.join(filename),
            format,
            &ImportOptions {
                batch_messages: 16,
                batch_bytes: 32 * 1024,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            imported.sources.len(),
            2,
            "Generated child transcript must be discovered"
        );
        assert_eq!(imported.stats.messages, messages_per_turn * total_turns);
        assert_eq!(imported.stats.user_messages, total_turns);
        assert_eq!(imported.stats.tool_calls, calls_per_turn * total_turns);
        assert_eq!(imported.stats.tool_messages, calls_per_turn * total_turns);
        assert_eq!(imported.stats.compactions, compactions);
        assert_eq!(
            imported.stats.derived_records,
            derived_per_turn * total_turns
                + compactions * if format == ImportFormat::Codex { 1 } else { 2 }
        );
        // Converter v2 keeps every reasoning artifact; each one carries one
        // opaque field (Codex encrypted_content, Claude redacted data).
        assert_eq!(
            imported.stats.reasoning_artifacts,
            reasoning_per_turn * total_turns
        );
        assert_eq!(imported.stats.opaque_encrypted_fields, total_turns);
        assert_eq!(imported.stats.encrypted_fields_dropped, 0);
        assert_eq!(imported.stats.excluded[excluded_kind], total_turns);
        if format == ImportFormat::Codex {
            assert_eq!(
                imported.stats.excluded["compaction_replacement_history_copy"],
                compactions
            );
            assert_eq!(
                imported.stats.excluded["compaction_guardian_history_copy"],
                compactions
            );
        }
        let chat = store.chat_path(&imported.root_session_id).unwrap().unwrap();
        let connection = rusqlite::Connection::open_with_flags(
            &chat,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        // Every turn uses the same large native result. Root and child share a
        // chat, so hundreds of complete tool outputs require one compressed blob.
        let (blobs, compressed_bytes): (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*),SUM(length(data)) FROM blobs WHERE raw_bytes=?1",
                [PAYLOAD_BYTES],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(blobs, 1);
        assert!(compressed_bytes < PAYLOAD_BYTES);
        // Each large reasoning artifact differs per turn, so each provider
        // state is its own blob referenced from the message row.
        let state_blobs: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE json_extract(data,'$._provider_state_blob') IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            u64::try_from(state_blobs).unwrap(),
            imported.stats.opaque_encrypted_fields
        );
        let blob_backed_tools: i64 = connection.query_row(
            "SELECT COUNT(*) FROM messages WHERE json_extract(data,'$.role')='tool' AND json_extract(data,'$.content')=''", [], |row| row.get(0)
        ).unwrap();
        assert_eq!(
            u64::try_from(blob_backed_tools).unwrap(),
            imported.stats.tool_messages
        );
        drop(connection);
        for source in &imported.sources {
            assert_eq!(store.chat_path(&source.session_id).unwrap().unwrap(), chat);
            verify_export(&store, source);
            let stored =
                serde_json::to_string(&store.messages(&source.session_id).unwrap()).unwrap();
            let marker = if format == ImportFormat::Codex {
                "SYNTHETIC_CIPHERTEXT_PRESERVED:"
            } else {
                "SYNTHETIC_REDACTED_PRESERVED:"
            };
            assert_eq!(
                stored.matches(marker).count() as u64,
                source.stats.opaque_encrypted_fields
            );
            if format == ImportFormat::Claude {
                assert_eq!(
                    stored.matches("synthetic-signature-exact+/=").count() as u64,
                    source.stats.reasoning_artifacts / 2
                );
            }
        }
        if format == ImportFormat::Codex {
            assert!(
                imported.sources[0].stats.messages
                    > u64::from(StoreOptions::default().cold_min_messages)
            );
            assert!(
                store
                    .compress_cold_messages_with_options(
                        &imported.root_session_id,
                        StoreOptions::default()
                    )
                    .unwrap()
                    > 0
            );
            for source in &imported.sources {
                verify_export(&store, source);
            }
        }
    }
}

#[test]
fn codex_preserves_original_native_content_and_records_source_compaction() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("codex.jsonl");
    write_source(
        &path,
        &[
            json!({"type":"session_meta","payload":{"id":"source-root"}}),
            response(
                json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"ambient"}]}),
            ),
            response(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>ambient</environment_context>Actual question"}]}),
            ),
            json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"Actual question"}]}}}),
            response(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Actual question"}]}),
            ),
            response(
                json!({"type":"function_call","call_id":"call-a","name":"run","namespace":"functions","arguments":"{\"cmd\":\"synthetic\"}","encrypted_function_args":"NEVER_ARCHIVE"}),
            ),
            response(
                json!({"type":"function_call_output","call_id":"call-a","output":[{"type":"text","text":"Native result"},{"type":"image","data":"synthetic-image"}]}),
            ),
            response(
                json!({"type":"custom_tool_call","call_id":"call-b","name":"patch","input":"native raw\ninput"}),
            ),
            response(json!({"type":"custom_tool_call_output","call_id":"call-b","output":"ok"})),
            response(
                json!({"type":"reasoning","summary":[{"type":"summary_text","text":"Plain reasoning"}],"encrypted_content":"NEVER_ARCHIVE"}),
            ),
            response(
                json!({"type":"web_search_call","id":"web-a","action":{"query":"Synthetic"},"status":"completed"}),
            ),
            response(
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer"}]}),
            ),
            json!({"type":"compacted","payload":{"message":"Source summary","replacement_history":[{"copy":"Do not duplicate"}],"retained_context":{"verified_answers":["Retain exact"]}}}),
        ],
    );
    let store = Store::open_data_dir(temp.path().join("store")).unwrap();
    let result = import_file(
        &store,
        &path,
        ImportFormat::Codex,
        &ImportOptions {
            batch_messages: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.stats.messages, 8);
    assert_eq!(result.stats.tool_calls, 2);
    assert_eq!(result.stats.tool_messages, 2);
    assert_eq!(result.stats.compactions, 1);
    assert_eq!(result.stats.reasoning_artifacts, 1);
    assert_eq!(result.stats.opaque_encrypted_fields, 2);
    assert_eq!(result.stats.encrypted_fields_dropped, 0);
    assert_eq!(result.stats.excluded["non_original_user"], 1);
    let messages = store.messages(&result.root_session_id).unwrap();
    assert_eq!(messages[0].content, "Actual question");
    assert_eq!(messages[1].tool_calls[0].name, "functions.run");
    assert_eq!(messages[3].tool_calls[0].arguments, "native raw\ninput");
    assert!(messages[2].content.contains("synthetic-image"));
    let derived = store.import_derived(&result.root_session_id).unwrap();
    // Reasoning is a message now, so the compaction is the only derived record.
    assert_eq!(derived.len(), 1);
    assert_eq!(derived[0].origin, "import");
    assert_eq!(
        derived[0].through_message_id.as_ref(),
        Some(&messages.last().unwrap().id)
    );
    assert_eq!(derived[0].content["message"], "Source summary");
    assert!(derived[0].content.get("replacement_history").is_none());
    // Opaque fields live in the messages' provider state, never in derived records.
    assert!(
        !serde_json::to_string(&derived)
            .unwrap()
            .contains("NEVER_ARCHIVE")
    );
    assert_eq!(
        messages[1].provider_state.as_ref().unwrap()["items"][0]["encrypted_function_args"],
        "NEVER_ARCHIVE"
    );
    let reasoning = messages
        .iter()
        .find_map(|message| {
            message
                .provider_state
                .as_ref()
                .filter(|state| state["items"][0]["type"] == "reasoning")
        })
        .unwrap();
    assert_eq!(reasoning["items"][0]["encrypted_content"], "NEVER_ARCHIVE");
    assert_eq!(reasoning["_bubblepaw_origin"]["provider_kind"], "responses");
    let receipt = store
        .import_metadata(&result.root_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(receipt["source_sha256"], result.sources[0].source_sha256);
    verify_export(&store, &result.sources[0]);
}

#[test]
fn real_typed_harness_looking_text_takes_precedence() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("real.jsonl");
    let text =
        "# AGENTS.md This is a real request. <system-reminder>Explain this tag</system-reminder>";
    write_source(
        &path,
        &[
            response(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
            ),
            json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":text}]}}}),
        ],
    );
    let store = Store::in_memory().unwrap();
    let result = import_file(&store, path, ImportFormat::Codex, &ImportOptions::default()).unwrap();
    assert_eq!(
        store.messages(&result.root_session_id).unwrap()[0].content,
        text
    );
}

#[test]
fn claude_signed_thinking_tools_images_and_source_summary_survive() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("claude.jsonl");
    write_source(
        &path,
        &[
            json!({"type":"user","sessionId":"claude-root","promptSource":"sdk","message":{"content":"Typed <system-reminder>real words</system-reminder>"}}),
            json!({"type":"user","isMeta":true,"message":{"content":"injected"}}),
            json!({"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Plain signed thought","signature":"exact+/=="},{"type":"redacted_thinking","data":"NEVER_ARCHIVE"},{"type":"text","text":"Answer"},{"type":"tool_use","id":"a","name":"tool","input":{"value":1},"caller":{"type":"direct"}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a","content":[{"type":"text","text":"result"},{"type":"image","source":{"type":"base64","data":"synthetic"}}],"is_error":true}]}}),
            json!({"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}}),
            json!({"type":"user","isCompactSummary":true,"message":{"role":"user","content":"Retained source summary"}}),
        ],
    );
    let store = Store::open_data_dir(temp.path().join("store")).unwrap();
    let result = import_file(
        &store,
        path,
        ImportFormat::Claude,
        &ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(result.stats.messages, 5);
    assert_eq!(result.stats.compactions, 1);
    assert_eq!(result.stats.tool_calls, 1);
    assert_eq!(result.stats.reasoning_artifacts, 2);
    assert_eq!(result.stats.opaque_encrypted_fields, 1);
    let messages = store.messages(&result.root_session_id).unwrap();
    assert!(messages[0].content.contains("real words"));
    // Thinking blocks become reasoning-only assistant records, in source order.
    let thinking = messages[1].provider_state.as_ref().unwrap();
    assert_eq!(
        thinking["content"][0],
        json!({"type":"thinking","thinking":"Plain signed thought","signature":"exact+/=="})
    );
    assert_eq!(
        thinking["_bubblepaw_origin"]["provider_kind"],
        "anthropic-messages"
    );
    assert_eq!(
        messages[2].provider_state.as_ref().unwrap()["content"][0],
        json!({"type":"redacted_thinking","data":"NEVER_ARCHIVE"})
    );
    assert_eq!(messages[3].content, "Answer");
    assert_eq!(messages[4].role, Role::Tool);
    assert!(messages[4].is_error);
    assert!(messages[4].content.contains("base64"));
    let derived = store.import_derived(&result.root_session_id).unwrap();
    assert!(
        !serde_json::to_string(&derived)
            .unwrap()
            .contains("NEVER_ARCHIVE")
    );
    assert_eq!(
        derived.last().unwrap().content["content"],
        "Retained source summary"
    );
    verify_export(&store, &result.sources[0]);
}

#[test]
fn adjacent_agents_share_one_root_chat_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("root.jsonl");
    write_source(
        &path,
        &[json!({"type":"user","sessionId":"same-native-id","message":{"content":"Parent"}})],
    );
    let agents = temp.path().join("root/subagents");
    std::fs::create_dir_all(&agents).unwrap();
    write_source(
        &agents.join("agent-one.jsonl"),
        &[json!({"type":"user","sessionId":"same-native-id","message":{"content":"Child"}})],
    );
    let store = Store::open_data_dir(temp.path().join("store")).unwrap();
    let result = import_file(
        &store,
        path,
        ImportFormat::Claude,
        &ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(result.sources.len(), 2);
    let child = store.session(&result.sources[1].session_id).unwrap();
    assert_eq!(
        child.parent_session_id.as_ref(),
        Some(&result.root_session_id)
    );
    assert_eq!(
        store.chat_path(&child.id).unwrap(),
        store.chat_path(&result.root_session_id).unwrap()
    );
}

#[test]
fn supplied_codex_descendants_keep_native_parent_links_in_any_order() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root.jsonl");
    let child = temp.path().join("child.jsonl");
    let grandchild = temp.path().join("grandchild.jsonl");
    for (path, id, parent) in [
        (&root, "native-root", None),
        (&child, "native-child", Some("native-root")),
        (&grandchild, "native-grandchild", Some("native-child")),
    ] {
        write_source(
            path,
            &[
                json!({"type":"session_meta","payload":{"id":id,"source":{"subagent":{"thread_spawn":{"parent_thread_id":parent}}}}}),
                response(
                    json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Original source prompt"}]}),
                ),
            ],
        );
    }
    let store = Store::open_data_dir(temp.path().join("store")).unwrap();
    let imported = import_file(
        &store,
        &root,
        ImportFormat::Codex,
        &ImportOptions {
            adjacent_agent_paths: vec![grandchild, child],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(imported.sources.len(), 3);
    let child = &imported.sources[1];
    let grandchild = &imported.sources[2];
    assert_eq!(child.source_session_id.as_deref(), Some("native-child"));
    assert_eq!(
        store
            .session(&grandchild.session_id)
            .unwrap()
            .parent_session_id
            .as_deref(),
        Some(child.session_id.as_str())
    );
    assert_eq!(
        store.chat_path(&grandchild.session_id).unwrap(),
        store.chat_path(&imported.root_session_id).unwrap()
    );
}

#[test]
fn codex_adjacent_discovery_imports_only_linked_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root.jsonl");
    for (name, id, parent) in [
        ("root", "native-root", None),
        ("child", "native-child", Some("native-root")),
        ("grandchild", "native-grandchild", Some("native-child")),
        ("unrelated", "unrelated", Some("different-root")),
    ] {
        write_source(
            &temp.path().join(format!("{name}.jsonl")),
            &[
                json!({"type":"session_meta","payload":{"id":id,"source":{"subagent":{"thread_spawn":{"parent_thread_id":parent}}}}}),
                response(
                    json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Source prompt"}]}),
                ),
            ],
        );
    }
    std::fs::write(temp.path().join("foreign.jsonl"), b"private-invalid-json\n").unwrap();
    let unrelated = std::fs::read(temp.path().join("unrelated.jsonl")).unwrap();
    let store = Store::open_data_dir(temp.path().join("store")).unwrap();
    let imported =
        import_file(&store, root, ImportFormat::Codex, &ImportOptions::default()).unwrap();
    assert_eq!(imported.sources.len(), 3);
    assert!(
        imported
            .sources
            .iter()
            .all(|source| source.source_session_id.as_deref() != Some("unrelated"))
    );
    assert_eq!(
        std::fs::read(temp.path().join("unrelated.jsonl")).unwrap(),
        unrelated
    );
    assert_eq!(
        store
            .session(&imported.sources[2].session_id)
            .unwrap()
            .parent_session_id
            .as_deref(),
        Some(imported.sources[1].session_id.as_str())
    );
}

#[test]
fn malformed_or_oversized_sources_create_no_sessions_and_hide_private_errors() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bad.jsonl");
    std::fs::write(&path, b"{\"SECRET_NEVER_PRINT\":bad}\n").unwrap();
    let store = Store::in_memory().unwrap();
    let error = import_file(
        &store,
        &path,
        ImportFormat::Codex,
        &ImportOptions::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(!error.contains("SECRET_NEVER_PRINT"));
    assert!(store.list_sessions().unwrap().is_empty());
    let error = import_file(
        &store,
        &path,
        ImportFormat::Codex,
        &ImportOptions {
            max_line_bytes: 4,
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("byte limit"));
    assert!(store.list_sessions().unwrap().is_empty());
}

fn verify_export(store: &Store, source: &bubblepaw_core::transcript::ImportedSource) {
    struct HashExport {
        bytes: Vec<u8>,
        hash: Sha256,
        count: u64,
        derived_hash: Sha256,
        derived_count: u64,
    }
    impl Write for HashExport {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut search_from = self.bytes.len();
            self.bytes.extend_from_slice(bytes);
            while let Some(offset) = self.bytes[search_from..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let end = search_from + offset;
                let line: Value = serde_json::from_slice(&self.bytes[..end]).unwrap();
                if let Some(value) = line.get("message") {
                    let message: Message = serde_json::from_value(value.clone()).unwrap();
                    self.hash
                        .update(serde_json::to_vec(&normalized_message(&message)).unwrap());
                    self.hash.update(b"\n");
                    self.count += 1;
                } else if let Some(value) = line.get("derived") {
                    let record = serde_json::from_value(value.clone()).unwrap();
                    self.derived_hash
                        .update(serde_json::to_vec(&normalized_derived(&record)).unwrap());
                    self.derived_hash.update(b"\n");
                    self.derived_count += 1;
                } else if line["kind"] == "import_provenance" {
                    assert!(line["import_provenance"].is_object());
                } else {
                    panic!("Unexpected export record kind");
                }
                self.bytes.drain(..=end);
                search_from = 0;
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut exported = HashExport {
        bytes: Vec::new(),
        hash: Sha256::new(),
        count: 0,
        derived_hash: Sha256::new(),
        derived_count: 0,
    };
    store
        .export_jsonl(&source.session_id, &mut exported)
        .unwrap();
    assert_eq!(exported.count, source.stats.messages);
    assert_eq!(
        bubblepaw_core::hex_lower(exported.hash.finalize()),
        source.normalized_sha256
    );
    assert_eq!(exported.derived_count, source.stats.derived_records);
    assert_eq!(
        bubblepaw_core::hex_lower(exported.derived_hash.finalize()),
        source.normalized_derived_sha256
    );
}

#[test]
fn compaction_only_sources_export_their_provenance_and_derived_content() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("compaction.jsonl");
    write_source(
        &path,
        &[
            json!({"type":"compacted","payload":{"message":"Original source summary","replacement_history":[]}}),
        ],
    );
    let store = Store::open_data_dir(temp.path().join("store")).unwrap();
    let imported = import_file(
        &store,
        &path,
        ImportFormat::Codex,
        &ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(imported.stats.messages, 0);
    assert_eq!(imported.stats.derived_records, 1);
    verify_export(&store, &imported.sources[0]);
}

#[test]
fn private_fixtures_counts_and_export_hashes() {
    let Some(directory) = std::env::var_os("BUBBLEPAW_FIXTURES_DIR") else {
        // Direct stderr stays visible under libtest's normal output capture.
        let _ = writeln!(
            std::io::stderr().lock(),
            "SKIP private transcript fixtures: BUBBLEPAW_FIXTURES_DIR is unset"
        );
        return;
    };
    for (file, format, expected_calls, expected_results, expected_compactions) in [
        (
            "codex-isaacnet.jsonl",
            ImportFormat::Codex,
            27211,
            27208,
            295,
        ),
        ("claude-long.jsonl", ImportFormat::Claude, 2067, 2068, 22),
    ] {
        let path = Path::new(&directory).join(file);
        assert!(
            path.is_file(),
            "Configured private fixture is missing: {file}"
        );
        // Independent shape pass knows only source native kinds; it never calls
        // the Rust normalizer or prints content.
        let mut calls = 0;
        let mut results = 0;
        let mut compactions = 0;
        for raw in BufReader::new(File::open(&path).unwrap()).lines() {
            let event: Value = serde_json::from_str(&raw.unwrap()).unwrap();
            if format == ImportFormat::Codex {
                match event["payload"]["type"].as_str().unwrap_or("") {
                    "function_call" | "custom_tool_call" => calls += 1,
                    "function_call_output" | "custom_tool_call_output" => results += 1,
                    _ => {}
                }
                if event["type"] == "compacted" {
                    compactions += 1;
                }
            } else {
                if event["subtype"] == "compact_boundary" {
                    compactions += 1;
                }
                if let Some(blocks) = event["message"]["content"].as_array() {
                    for block in blocks {
                        match block["type"].as_str().unwrap_or("") {
                            "tool_use" => calls += 1,
                            "tool_result" => results += 1,
                            _ => {}
                        }
                    }
                }
            }
        }
        assert_eq!(
            (calls, results, compactions),
            (expected_calls, expected_results, expected_compactions)
        );
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open_data_dir(temp.path().join("store")).unwrap();
        let began = std::time::Instant::now();
        let imported = import_file(
            &store,
            &path,
            format,
            &ImportOptions {
                discover_adjacent_agents: false,
                ..Default::default()
            },
        )
        .unwrap();
        let import_seconds = began.elapsed().as_secs_f64();
        assert_eq!(imported.stats.tool_calls, calls);
        assert_eq!(imported.stats.tool_messages, results);
        assert_eq!(imported.stats.compactions, compactions);
        if format == ImportFormat::Codex {
            // 65,346 conversation messages plus 25,722 kept reasoning items.
            assert_eq!(imported.stats.messages, 91068);
            assert_eq!(imported.stats.user_messages, 140);
            assert_eq!(imported.stats.reasoning_artifacts, 25722);
        } else {
            assert_eq!(imported.stats.reasoning_artifacts, 1721);
        }
        assert_eq!(imported.stats.encrypted_fields_dropped, 0);
        let export_began = std::time::Instant::now();
        verify_export(&store, &imported.sources[0]);
        let export_seconds = export_began.elapsed().as_secs_f64();
        eprintln!(
            "Verified private fixture {file}: {} messages, {calls} calls, {results} results, {compactions} compactions; normalized export hash {}",
            imported.stats.messages, imported.sources[0].normalized_sha256
        );
        eprintln!(
            "{}",
            json!({"fixture":file,"stats":imported.stats,"import_seconds":import_seconds,"export_seconds":export_seconds,
            "normalized_sha256":imported.sources[0].normalized_sha256,"normalized_derived_sha256":imported.sources[0].normalized_derived_sha256})
        );
    }
}
