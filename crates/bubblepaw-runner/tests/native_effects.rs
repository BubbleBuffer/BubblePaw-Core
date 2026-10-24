use bubblepaw_core::{Harness, HarnessConfig, Role, RunOptions, Store, ToolCall, ToolRegistry};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    provider::{ScriptedProvider, ScriptedStep},
    tools::register_native_tools,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[cfg(windows)]
const SLEEP: &str = "Start-Sleep -Seconds 30";
#[cfg(not(windows))]
const SLEEP: &str = "sleep 30";

fn provider(name: &str, arguments: Value) -> Arc<ScriptedProvider> {
    let arguments = if name == "js" {
        arguments
    } else {
        json!({"code":format!("const result = await tools.{name}({arguments}); text(JSON.stringify(result));")})
    };
    Arc::new(ScriptedProvider::with_steps(
        vec![ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "js".into(),
                arguments,
            }],
        }],
        "finished",
    ))
}

#[tokio::test]
async fn shared_dispatch_preserves_json_cursors_and_escaped_reads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("control.txt"), vec![0; 20000]).unwrap();
    let store = Store::in_memory().unwrap();
    let session = store.create_session("bounded JSON").unwrap();
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, root.path(), false).unwrap();
    let repl = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    repl.register(&mut tools).unwrap();
    #[cfg(windows)]
    let command = "[Console]::Out.Write((([string][char]0) * 180000)); [Console]::Error.Write((([string][char]0) * 180000))";
    #[cfg(not(windows))]
    let command = "head -c 180000 /dev/zero; head -c 180000 /dev/zero >&2";
    let code = format!(
        "const process = await tools.exec({}); await wait({{handle:process.handle}}); const out=await tools.poll({{handle:process.handle}}); if(typeof out !== 'object' || out.cursor.stdout !== 180000 || out.cursor.stderr !== 180000) throw Error('lost process object'); const next=await tools.poll({{handle:process.handle,cursor:out.cursor}}); if(next.stdout !== '' || next.stderr !== '') throw Error('duplicate output'); const read=await tools.read({{path:'control.txt'}}); if(typeof read !== 'object' || read.content.length !== 4096 || !read.truncated) throw Error('lost read object'); text(JSON.stringify({{stdout:out.stdout.length,stderr:out.stderr.length,stdoutMarker:out.stdout.includes('omitted bytes'),stderrMarker:out.stderr.includes('omitted bytes'),gaps:out.capture.stdout.gaps,read:read.content.length}}));",
        json!({"command":command,"yieldMs":0})
    );
    let harness = Harness::new(
        store.clone(),
        provider("js", json!({"code":code})),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            // Two 16 KiB NUL previews expand past the default envelope in JSON.
            // Keep this cursor/object test below its explicitly larger host limit.
            max_tool_result_bytes: 256 * 1024,
            ..Default::default()
        },
    );
    assert_eq!(
        harness
            .prompt(&session.id, "start", RunOptions::default())
            .await
            .unwrap()
            .status,
        "completed"
    );
    let outputs = store.outputs(&session.id, 10).unwrap();
    let text = outputs
        .iter()
        .find_map(|o| o.output["text"].as_str())
        .unwrap_or_else(|| {
            let errors = store
                .messages(&session.id)
                .unwrap()
                .into_iter()
                .filter(|message| message.role == Role::Tool && message.is_error)
                .map(|message| message.content)
                .collect::<Vec<_>>();
            panic!("JS failed to emit result; stored tool errors: {errors:?}");
        });
    let output: Value = serde_json::from_str(text).unwrap();
    for stream in ["stdout", "stderr"] {
        let length = output[stream].as_u64().unwrap();
        assert!(length > 0 && length <= 16 * 1024);
    }
    assert_eq!(output["stdoutMarker"], true);
    assert_eq!(output["stderrMarker"], true);
    assert_eq!(output["read"], 4096);
    assert_eq!(output["gaps"], json!([{"from":8064,"to":171936}]));
    assert!(store.pending_effects(&session.id).unwrap().is_empty());
    repl.shutdown().await;
    processes.shutdown().await;
}

#[tokio::test]
async fn js_exec_keeps_full_host_object_and_returns_only_exit_code() {
    let root = tempfile::tempdir().unwrap();
    let stdout = "0123456789".repeat(1639)[..16 * 1024].to_owned();
    let stderr = "9876543210".repeat(1639)[..16 * 1024].to_owned();
    std::fs::write(root.path().join("stdout.txt"), &stdout).unwrap();
    std::fs::write(root.path().join("stderr.txt"), &stderr).unwrap();
    let store = Store::in_memory().unwrap();
    let session = store.create_session("small JS reply").unwrap();
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, root.path(), false).unwrap();
    let repl = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    repl.register(&mut tools).unwrap();
    #[cfg(windows)]
    let command = "[Console]::Out.Write([IO.File]::ReadAllText('stdout.txt')); [Console]::Error.Write([IO.File]::ReadAllText('stderr.txt'))";
    #[cfg(not(windows))]
    let command = "cat stdout.txt; cat stderr.txt >&2";
    let code = format!(
        "const r = await tools.exec({}); if (r.status !== 'exited' || r.stdout !== {} || r.stderr !== {} || r.cursor.stdout !== 16384 || r.cursor.stderr !== 16384 || r.capture.stdout.gaps.length || r.capture.stderr.gaps.length) throw Error('lost exec result object'); return r.exitCode;",
        json!({"command":command}),
        json!(stdout),
        json!(stderr),
    );
    let harness = Harness::new(
        store.clone(),
        provider("js", json!({"code":code})),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    harness
        .prompt(&session.id, "start", RunOptions::default())
        .await
        .unwrap();
    let messages = store.messages(&session.id).unwrap();
    let reply = messages.iter().find(|m| m.role == Role::Tool).unwrap();
    assert!(!reply.is_error, "{}", reply.content);
    assert_eq!(reply.content, "0");
    assert!(store.pending_effects(&session.id).unwrap().is_empty());
    repl.shutdown().await;
    processes.shutdown().await;
}

#[tokio::test]
async fn js_running_process_keeps_boundary_and_blocks_fork_until_terminal() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let session = store.create_session("active process").unwrap();
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, root.path(), false).unwrap();
    let repl = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    repl.register(&mut tools).unwrap();
    let harness = Harness::new(
        store.clone(),
        provider("exec", json!({"command":SLEEP,"yieldMs":0})),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    assert_eq!(
        harness
            .prompt(&session.id, "launch", RunOptions::default())
            .await
            .unwrap()
            .status,
        "completed"
    );
    let pending = store.pending_effects(&session.id).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["state"], "dispatched");
    assert!(store.fork(&session.id, "blocked fork").is_err());
    assert!(harness.cancel(&session.id));
    processes.shutdown().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !store.pending_effects(&session.id).unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(store.fork(&session.id, "terminal fork").is_ok());
}

#[test]
fn pending_completed_run_recovers_unknown_without_replaying_process() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (session, repl) = runtime.block_on(async {
        let store = Store::open(&path).unwrap();
        let session = store.create_session("crash boundary").unwrap();
        let mut tools = ToolRegistry::new();
        let _processes = register_native_tools(&mut tools, root.path(), false).unwrap();
        let repl = Arc::new(
            CodeMode::new(
                root.path().to_owned(),
                Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
            )
            .unwrap(),
        );
        repl.register(&mut tools).unwrap();
        let harness = Harness::new(
            store.clone(),
            provider("exec", json!({"command":SLEEP,"yieldMs":0})),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        assert_eq!(
            harness
                .prompt(&session.id, "launch", RunOptions::default())
                .await
                .unwrap()
                .status,
            "completed"
        );
        assert_eq!(
            store.pending_effects(&session.id).unwrap()[0]["state"],
            "dispatched"
        );
        (session.id, repl)
    });
    // Tear down executor before its completion observer runs, as on interrupted host exit.
    drop(runtime);
    // Join the REPL thread on a fresh executor. The old completion observer has
    // been destroyed, so cleanup cannot settle the dispatched effect. A live
    // REPL could otherwise retain a Store and make open reuse the old host.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(repl.shutdown());
    drop(repl);
    let recovered = Store::open(&path).unwrap();
    let pending = recovered.pending_effects(&session).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["state"], "unknown");
    assert_eq!(
        recovered
            .messages(&session)
            .unwrap()
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        1
    );
}
