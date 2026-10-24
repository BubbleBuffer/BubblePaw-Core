use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, CancellationToken, Decision, EventKind, Harness, HarnessConfig,
    ModelRequest, ModelResponse, Provider, Role, RunOptions, Store, TextCallback, Tool, ToolCall,
    ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore, codemode::CodeMode, tools::register_native_tools,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Cells;
#[async_trait]
impl Provider for Cells {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert_eq!(
            request
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["js"]
        );
        if let Some(last) = request.messages.last().filter(|m| m.role == Role::User)
            && let Ok(arguments) = serde_json::from_str::<Value>(&last.content)
        {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments,
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "finished".into(),
            ..Default::default()
        })
    }
}
struct Native(&'static str, Arc<AtomicUsize>);
#[async_trait]
impl Tool for Native {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: self.0.into(),
            description: "Test capability".into(),
            read_only: true,
            input_schema: json!({"type":"object"}),
        }
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        self.1.fetch_add(1, Ordering::Relaxed);
        match self.0 {
            "echo" => {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(ToolResult::text(
                    json!({"value":input["value"],"invocation":context.invocation_id}).to_string(),
                ))
            }
            "exec" => Ok(ToolResult::text(input.to_string())),
            "wait" => {
                tokio::time::sleep(Duration::from_millis(input["ms"].as_u64().unwrap())).await;
                Ok(ToolResult::text(json!({"status":"elapsed"}).to_string()))
            }
            "stall" => std::future::pending().await,
            "reject" => Ok(ToolResult::error("deliberate rejection")),
            _ => unreachable!(),
        }
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    harness: Harness,
    registry: ToolRegistry,
    manager: Arc<CodeMode>,
    session: String,
    _processes: Option<Arc<bubblepaw_runner::tools::ProcessTools>>,
    calls: Arc<AtomicUsize>,
}
impl Fixture {
    fn new() -> Self {
        Self::build(false, false)
    }
    fn with_native_tools() -> Self {
        Self::build(true, false)
    }
    fn build(native: bool, read_only: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("report.txt"), "capture me").unwrap();
        let artifacts =
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
        let manager = Arc::new(CodeMode::new(root.path().to_owned(), artifacts).unwrap());
        let mut tools = ToolRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let processes =
            native.then(|| register_native_tools(&mut tools, root.path(), false).unwrap());
        for name in ["echo", "stall", "reject"] {
            tools
                .register_host(Arc::new(Native(name, calls.clone())))
                .unwrap();
        }
        if !native {
            for name in ["wait", "exec"] {
                tools
                    .register_host(Arc::new(Native(name, calls.clone())))
                    .unwrap();
            }
        }
        if read_only {
            tools = tools.read_only();
        }
        manager.register(&mut tools).unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("cells").unwrap().id;
        let harness = Harness::new(
            store.clone(),
            Arc::new(Cells),
            tools.clone(),
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Self {
            _root: root,
            store,
            harness,
            registry: tools,
            manager,
            session,
            _processes: processes,
            calls,
        }
    }
    async fn cell(&self, env: &str, code: &str) -> Value {
        self.cell_input(json!({"environment":env,"code":code}))
            .await
    }
    async fn cell_input(&self, input: Value) -> Value {
        self.harness
            .prompt(&self.session, input.to_string(), RunOptions::default())
            .await
            .unwrap();
        let messages = self.store.messages(&self.session).unwrap();
        let last = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(!last.is_error, "{}", last.content);
        serde_json::from_str(last.content.split("\n[---]\n").next().unwrap()).unwrap()
    }
    async fn cell_error(&self, code: &str) -> String {
        self.harness
            .prompt(
                &self.session,
                json!({"code":code}).to_string(),
                RunOptions::default(),
            )
            .await
            .unwrap();
        let messages = self.store.messages(&self.session).unwrap();
        let last = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(last.is_error, "{}", last.content);
        last.content.clone()
    }
    fn texts(&self) -> Vec<String> {
        self.store
            .outputs(&self.session, 100)
            .unwrap()
            .into_iter()
            .filter_map(|r| r.output["text"].as_str().map(str::to_owned))
            .collect()
    }
    fn latest_tool_content(&self) -> String {
        self.store
            .messages(&self.session)
            .unwrap()
            .into_iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap()
            .content
    }
    async fn until(&self, mut condition: impl FnMut(&Value) -> bool) {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let status = self.manager.status(&self.session);
                if condition(&status["environments"][0]) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("environment state deadline");
    }
}

#[tokio::test]
async fn state_changes_and_reset_are_compact_model_notices_without_publication() {
    let f = Fixture::new();
    assert_eq!(
        f.cell(
            "main",
            "state.docs={items:[1]}; state.temp='secret-state-value'; return {ok:true};"
        )
        .await,
        json!({"ok":true})
    );
    assert_eq!(
        f.latest_tool_content(),
        "{\"ok\":true}\n[---]\nstate.docs added\nstate.temp added"
    );
    f.cell("main", "state.docs.items.push(2); return 1;").await;
    assert_eq!(f.latest_tool_content(), "1\n[---]\nstate.docs changed");
    f.cell("main", "delete state.temp; return 2;").await;
    assert_eq!(f.latest_tool_content(), "2\n[---]\nstate.temp deleted");
    assert_eq!(
        f.cell("main", "state.reset(); return state;").await,
        json!({})
    );
    assert_eq!(
        f.latest_tool_content(),
        "{}\n[---]\nstate reset\nstate.docs deleted"
    );
    assert_eq!(
        f.cell("main", "state.reset(); return Object.keys(state);")
            .await,
        json!([])
    );
    assert_eq!(f.latest_tool_content(), "[]\n[---]\nstate reset");
    f.cell("main", "return 42;").await;
    assert_eq!(f.latest_tool_content(), "42");
    assert!(f.texts().is_empty());
    f.manager.shutdown().await;
}

#[tokio::test]
async fn limited_state_observation_does_not_invent_root_additions_after_key_shift() {
    let f = Fixture::new();
    f.cell("main", "for(let i=0;i<70;i++) state['k'+i]=i; return 0;")
        .await;
    f.cell("main", "delete state.k0; return 1;").await;
    let content = f.latest_tool_content();
    assert!(content.contains("state.k0 deleted"), "{content}");
    assert!(!content.contains("state.k64 added"), "{content}");
    assert!(content.contains("state observation limited"), "{content}");
    f.manager.shutdown().await;
}

#[tokio::test]
async fn state_observation_never_invokes_getters_and_reset_preflights_all_keys() {
    let f = Fixture::new();
    f.cell("main","state.docs={get hidden(){text('nested getter ran');throw Error('getter');}}; Object.defineProperty(state,'getter',{get(){text('top getter ran');throw Error('getter');},configurable:true,enumerable:true}); return 0;").await;
    f.cell("main", "return 1;").await;
    assert_eq!(f.latest_tool_content(), "1");
    f.cell("main","state.reset(); state.a=1; Object.defineProperty(state,'locked',{value:2,enumerable:true}); return 2;").await;
    assert_eq!(f.cell("main","try {state.reset();} catch(e) {if(!e.message.includes('no keys were deleted')) throw e;} return {a:state.a,locked:state.locked};").await,json!({"a":1,"locked":2}));
    assert!(!f.latest_tool_content().contains("[---]"));
    let error = f.cell_error("state.reset();").await;
    assert!(
        error.contains("non-configurable") && error.contains("state reset (failed/cancelled cell)"),
        "{error}"
    );
    assert_eq!(
        f.cell("main", "return Object.keys(state);").await,
        json!([])
    );
    assert!(
        f.latest_tool_content()
            .contains("state reset (previous failed/cancelled cell)")
    );
    assert!(f.texts().is_empty());
    f.manager.shutdown().await;
}

#[tokio::test]
async fn state_notice_keys_are_escaped_and_byte_bounded_and_errors_keep_reset_notice() {
    let f = Fixture::new();
    f.cell(
        "main",
        "state['\\n[---]\\nkey']='never-print-secret-value'; return 0;",
    )
    .await;
    let content = f.latest_tool_content();
    assert_eq!(content.matches("\n[---]\n").count(), 1);
    assert!(content.contains("state[\"\\n[---]\\nkey\"] added"));
    assert!(!content.contains("never-print-secret-value"));
    f.cell(
        "main",
        "for(let i=0;i<64;i++) state['🐾'.repeat(70)+i]=i; return 1;",
    )
    .await;
    let content = f.latest_tool_content();
    assert!(content.len() < 2200, "{}", content.len());
    assert!(content.contains("notices omitted"));
    let error = f.cell_error("throw Error('🐾'.repeat(100000));").await;
    assert!(error.ends_with("\n[---]\nstate reset (failed/cancelled cell)"));
    assert!(error.len() < 64 * 1024);
    f.manager.shutdown().await;
}

#[tokio::test]
async fn expression_and_body_results_are_model_data_and_publication_is_explicit() {
    let f = Fixture::with_native_tools();
    let content = "a complete result beyond the publication preview. ".repeat(40);
    std::fs::write(f._root.path().join("long.txt"), &content).unwrap();
    let result = f.cell("main", "await tools.read({path:'long.txt'});").await;
    assert_eq!(result["content"], content);
    assert!(f.texts().is_empty());
    let body=f.cell("main", "const read=await tools.read({path:'long.txt'}); await wait({ms:1}); return {count:read.content.length,content:read.content};").await;
    assert_eq!(body["content"], content);
    assert_eq!(body["count"], content.len());
    assert_eq!(
        f.cell("main", "({number:3,items:[true,null,'value']})")
            .await,
        json!({"number":3,"items":[true,null,"value"]})
    );
    assert_eq!(f.cell("main", "null").await, Value::Null);
    assert_eq!(
        f.cell("main", "'plain string'").await,
        json!("plain string")
    );
    assert!(f.texts().is_empty());
    assert_eq!(
        f.cell("main", "text('published'); return 42;").await,
        json!(42)
    );
    assert_eq!(f.texts(), ["published"]);
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn long_text_output_arrives_whole_within_the_environment_cap() {
    let f = Fixture::new();
    let long = "a long published line of text. ".repeat(300);
    f.cell(
        "main",
        &format!("text({}); return 1;", serde_json::to_string(&long).unwrap()),
    )
    .await;
    let inspection = f
        .manager
        .control(&f.session, json!({"action":"inspect","environment":"main"}))
        .unwrap();
    assert_eq!(inspection["outputs"][0]["text"], long);
    assert!(inspection["outputs"][0].get("text_truncated").is_none());
    assert_eq!(inspection["outputs_omitted"], 0);
    f.manager.shutdown().await;
}

#[tokio::test]
async fn oversized_output_sets_stay_bounded_and_report_omitted_outputs() {
    let f = Fixture::new();
    let chunk = "b".repeat(4 * 1024);
    f.cell(
        "main",
        &format!(
            "const chunk={}; for(let i=0;i<8;i++) text(i+chunk); return 1;",
            serde_json::to_string(&chunk).unwrap()
        ),
    )
    .await;
    let inspection = f
        .manager
        .control(&f.session, json!({"action":"inspect","environment":"main"}))
        .unwrap();
    let outputs = inspection["outputs"].as_array().unwrap();
    let omitted = inspection["outputs_omitted"].as_u64().unwrap() as usize;
    assert_eq!(inspection["output_count"], 8);
    assert!(omitted > 0, "{inspection}");
    assert_eq!(outputs.len() + omitted, 8);
    let kept_bytes: usize = outputs.iter().map(|o| o.to_string().len()).sum();
    assert!(kept_bytes <= 24 * 1024, "{kept_bytes}");
    assert_eq!(outputs[0]["text"].as_str().unwrap().len(), 4 * 1024 + 1);
    assert!(outputs.iter().all(|o| o.get("text_truncated").is_none()));
    let listed = f.manager.status(&f.session);
    assert_eq!(listed["environments"][0]["outputs_omitted"], omitted);
    f.manager.shutdown().await;
}

#[tokio::test]
async fn undefined_object_fields_are_omitted_without_resetting_successful_state() {
    let f = Fixture::new();
    assert_eq!(
        f.cell("main", "state.saved=42; const nested=Object.create(null); nested.optional=undefined; nested.value=null; return {optional:undefined,nested,items:[{optional:undefined,value:1}]};").await,
        json!({"nested":{"value":null},"items":[{"value":1}]})
    );
    assert_eq!(f.cell("main", "state.saved").await, 42);
    let receipt = f.cell("main", "undefined").await;
    assert!(receipt["execution_id"].as_str().is_some());
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["generation"],
        1
    );
    assert!(f.texts().is_empty());
    f.manager.shutdown().await;
}

#[tokio::test]
async fn deep_optional_fields_reject_at_resource_limits_without_side_effects() {
    let f = Fixture::new();
    f.cell("main", "state.beforeDepthCheck='discard me'; return true;")
        .await;
    let generation = f.manager.status(&f.session)["environments"][0]["generation"]
        .as_u64()
        .unwrap();
    let error = f
        .cell_error(
            "let value={optional:undefined}; for(let i=0;i<128;i++) value={value}; return value;",
        )
        .await;
    // QuickJS's bounded stack may reject before the encoder's depth cap.
    assert!(
        error.contains("structure limit exceeded")
            || error.contains("Maximum call stack size exceeded"),
        "{error}"
    );
    assert!(error.contains("state reset (failed/cancelled cell)"));
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["generation"],
        generation + 1
    );
    assert_eq!(
        f.cell("main", "typeof state.beforeDepthCheck").await,
        "undefined"
    );
    assert_eq!(f.calls.load(Ordering::Relaxed), 0);
    assert!(f.texts().is_empty());
    f.manager.shutdown().await;
}

#[tokio::test]
async fn successful_exec_optional_diagnostic_can_be_returned_directly() {
    let f = Fixture::with_native_tools();
    #[cfg(windows)]
    let command = "[Console]::Out.Write('optional-field-ok')";
    #[cfg(not(windows))]
    let command = "printf optional-field-ok";
    let result = f.cell("main", &format!("const r=await tools.exec({{command:{}}}); if(r.diagnostic!==undefined) throw Error('unexpected diagnostic'); state.completed=r.status; return {{status:r.status,diagnostic:r.diagnostic,stdout:r.stdout}};", json!(command))).await;
    assert_eq!(
        result,
        json!({"status":"exited","stdout":"optional-field-ok"})
    );
    assert_eq!(f.cell("main", "state.completed").await, "exited");
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["generation"],
        1
    );
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn ordinary_local_names_can_shadow_helpers_without_changing_their_apis() {
    let f = Fixture::new();
    assert_eq!(
        f.cell("main", "'use strict'; const cmd='command', text='local text', tool='local tool', tools='local tools', state='local state', file='local file', image='local image', wait='local wait'; return {cmd,text,tool,tools,state,file,image,wait};").await,
        json!({"cmd":"command","text":"local text","tool":"local tool","tools":"local tools","state":"local state","file":"local file","image":"local image","wait":"local wait"})
    );
    let result = f.cell("main", "const tool=tools.echo; const r=await tool({value:21}); await wait({ms:1}); const artifact=await file({path:'report.txt'}); text(artifact.name); state.oldCallback=()=>text('wrong owner'); state.oldTool=()=>tools.echo({value:'wrong owner'}); return {value:r.value,command:cmd`probe ${21}`,imageType:typeof image};").await;
    assert_eq!(result["value"], 21);
    assert_eq!(
        result["command"],
        json!({"parts":["probe ",""],"values":["21"]})
    );
    assert_eq!(result["imageType"], "function");
    let ownership = f.cell("main", "const errors=[]; try {state.oldCallback();} catch(e) {errors.push(e.message);} try {await state.oldTool();} catch(e) {errors.push(e.message);} return errors;").await;
    assert_eq!(
        ownership,
        json!([
            "Cell no longer owns output",
            "Cell no longer owns this capability"
        ])
    );
    assert_eq!(f.calls.load(Ordering::Relaxed), 2);
    assert_eq!(f.texts(), ["report.txt"]);
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["generation"],
        1
    );
    // The body's own directive must remain effective after helper injection.
    assert_eq!(f.cell("main", "'use strict'; try {undeclaredLocal=1; return false;} catch(e) {return e instanceof ReferenceError;}").await, true);
    assert_eq!(
        f.cell("main", "'use strict'; return this===undefined;")
            .await,
        true
    );
    assert_eq!(f.cell("main", "return this===globalThis;").await, true);
    f.manager.shutdown().await;
}

fn assert_output_budget(content: &str, max_tokens: usize) {
    let tokenizer =
        tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase).unwrap();
    assert!(content.len() <= 60 * 1024, "{} bytes", content.len());
    assert!(
        tokenizer.encode_ordinary(content).len() <= max_tokens,
        "{} tokens",
        tokenizer.encode_ordinary(content).len()
    );
}

#[tokio::test]
async fn dense_results_are_bounded_successful_previews_with_full_access_and_saved_state() {
    let f = Fixture::with_native_tools();
    let result = f
        .cell(
            "main",
            "state.full={body:'x '.repeat(24000),tail:'complete'}; return state.full;",
        )
        .await;
    assert_eq!(result["truncated"], true);
    assert_eq!(result["previewFormat"], "text");
    assert!(
        result["preview"]
            .as_str()
            .unwrap()
            .contains("output preview truncated")
    );
    assert_eq!(result["stateChanges"], json!(["state.full added"]));
    assert_output_budget(&f.latest_tool_content(), 10000);
    let path = result["fullOutput"]["path"].as_str().unwrap();
    let full = std::fs::read_to_string(f._root.path().join(path)).unwrap();
    assert_eq!(
        full.len(),
        result["fullOutput"]["bytes"].as_u64().unwrap() as usize
    );
    assert!(full.len() < 60 * 1024);
    assert_eq!(
        serde_json::from_str::<Value>(full.split("\n[---]\n").next().unwrap()).unwrap()["body"],
        "x ".repeat(24000)
    );
    assert_eq!(f.cell("main", "state.full.body.length").await, 48000);
    // The reference survives the next cell clearing the volatile last_result.
    let read = f.cell("main", &format!("const r=await tools.read({{path:{}}}); return {{bytes:r.content.length,tail:JSON.parse(r.content.split('\\n[---]\\n')[0]).tail}};", json!(path))).await;
    assert_eq!(read["tail"], "complete");
    assert_eq!(read["bytes"], full.len());
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["generation"],
        1
    );
    assert!(f.texts().is_empty());
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn output_overrides_cover_encoded_json_notices_unicode_and_byte_backstop() {
    let f = Fixture::new();
    let low = f.cell_input(json!({"maxOutputTokens":256,"code":"for(let i=0;i<64;i++) state['🐾'.repeat(30)+i]=i; return {body:'🐾\\\"\\n'.repeat(6000)};"})).await;
    assert_eq!(low["truncated"], true);
    assert_eq!(low["metadataTruncated"], true);
    assert_output_budget(&f.latest_tool_content(), 256);
    assert_eq!(low["previewFormat"], "text");
    let larger = f
        .cell_input(json!({"maxOutputTokens":25000,"code":"'x '.repeat(20000)"}))
        .await;
    assert_eq!(larger, "x ".repeat(20000));
    assert_output_budget(&f.latest_tool_content(), 25000);
    let bytes = f
        .cell_input(json!({"maxOutputTokens":1000000,"code":"'abcdefgh'.repeat(20000)"}))
        .await;
    assert_eq!(bytes["truncated"], true);
    assert_output_budget(&f.latest_tool_content(), 1000000);
    let full = std::fs::read_to_string(
        f._root
            .path()
            .join(bytes["fullOutput"]["path"].as_str().unwrap()),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<String>(&full).unwrap(),
        "abcdefgh".repeat(20000)
    );
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["generation"],
        1
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn invalid_output_budget_has_no_effect_and_spill_failure_keeps_successful_state() {
    let f = Fixture::new();
    for value in [
        json!(0),
        json!(255),
        json!(-1),
        json!(256.5),
        json!("1000"),
        Value::Null,
    ] {
        f.harness.prompt(&f.session, json!({"maxOutputTokens":value,"code":"await tools.echo({value:'must not execute'}); state.changed=true;"}).to_string(), RunOptions::default()).await.unwrap();
        assert!(f.latest_tool_content().contains("maxOutputTokens"));
    }
    assert_eq!(f.calls.load(Ordering::Relaxed), 0);
    assert!(
        f.manager.status(&f.session)["environments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    std::fs::write(f._root.path().join("spill"), "spill parent is a file").unwrap();
    let result = f
        .cell_input(
            json!({"maxOutputTokens":256,"code":"state.saved=42; return 'x '.repeat(20000);"}),
        )
        .await;
    assert!(
        result["fullOutputError"]
            .as_str()
            .unwrap()
            .contains("Could not save full output")
    );
    assert!(result.get("fullOutput").is_none());
    assert_output_budget(&f.latest_tool_content(), 256);
    assert_eq!(f.cell("main", "state.saved").await, 42);
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["generation"],
        1
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn execution_receipts_and_combined_environment_inspection_are_bounded() {
    let f = Fixture::new();
    for environment in ["main", "a", "b", "c"] {
        if environment != "main" {
            f.manager
                .control(
                    &f.session,
                    json!({"action":"create","environment":environment}),
                )
                .unwrap();
        }
        let receipt = f.cell_input(json!({"environment":environment,"maxOutputTokens":256,"code":"for(let i=0;i<64;i++) {state['🐾'.repeat(30)+i]=i; text('🐾'.repeat(100));}"})).await;
        assert_eq!(receipt["truncated"], true);
        assert_output_budget(&f.latest_tool_content(), 256);
    }
    let inspection = f
        .manager
        .control(&f.session, json!({"action":"inspect"}))
        .unwrap();
    assert_output_budget(&inspection.to_string(), 10000);
    assert_eq!(inspection["truncated"], true);
    f.manager.shutdown().await;
}

#[tokio::test]
async fn result_errors_are_clear_and_compile_fallback_never_replays_effects() {
    let f = Fixture::new();
    let error = f
        .cell_error("(await tools.echo({value:'once'}), (()=>{throw Error('runtime once');})())")
        .await;
    assert!(
        error.starts_with("runtime once (line 1, column "),
        "{error}"
    );
    assert!(error.ends_with("state reset (failed/cancelled cell)"));
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
    let error = f
        .cell_error("await tools.echo({value:'body once'}); throw Error('body failure');")
        .await;
    assert!(
        error.starts_with("body failure (line 1, column "),
        "{error}"
    );
    assert!(error.ends_with("state reset (failed/cancelled cell)"));
    assert_eq!(f.calls.load(Ordering::Relaxed), 2);
    assert!(
        f.cell_error("1); await tools.echo({value:'must not run'}); (2")
            .await
            .contains("expect")
    );
    assert_eq!(f.calls.load(Ordering::Relaxed), 2);
    for code in [
        "return [undefined];",
        "return Array(1);",
        "const value=[]; value.extra=undefined; return value;",
        "return Object.defineProperty({},'value',{value:undefined});",
        "return Object.defineProperty({},'value',{get(){text('getter must not run');return undefined;},enumerable:true});",
        "return {[Symbol('value')]:undefined};",
        "return Object.assign(Object.create({}),{optional:undefined});",
        "return {toJSON(){text('toJSON must not run');return {};}};",
        "return NaN;",
        "return Infinity;",
        "return 1n;",
        "return ()=>1;",
        "const value={}; value.self=value; return value;",
        "return {get value(){text('getter must not run');return 1;}};",
        "return new Date();",
        "const item=Object.fromEntries(Array.from({length:100},(_,i)=>[i,undefined])); return Array(1000).fill(item);",
    ] {
        let error = f.cell_error(code).await;
        assert!(error.contains("JSON-compatible"), "{code}: {error}");
    }
    let error = f
        .cell_error("await tools.echo({value:'serialize once'}); return {fn:()=>1};")
        .await;
    assert!(error.contains("JSON-compatible"));
    assert_eq!(f.calls.load(Ordering::Relaxed), 3);
    let large = f.cell("main", "'x'.repeat(1048577)").await;
    assert_eq!(large["truncated"], true);
    let full = std::fs::read_to_string(large["fullOutput"]["path"].as_str().unwrap()).unwrap();
    assert_eq!(
        serde_json::from_str::<String>(full.split("\n[---]\n").next().unwrap())
            .unwrap()
            .len(),
        1048577
    );
    assert_eq!(f.cell("main", "'corrected'").await, json!("corrected"));
    assert!(f.texts().is_empty());
    f.manager.shutdown().await;
}

#[tokio::test]
async fn eventual_background_result_is_inspectable_without_publication() {
    let f = Fixture::new();
    f.cell(
        "main",
        "await tools.environments({action:'create',environment:'monitor'});",
    )
    .await;
    f.harness
        .start(
            &f.session,
            json!({"code":"await wait({ms:60000}); state.finished=true; return {answer:'eventual'};"}).to_string(),
            RunOptions::default(),
        )
        .unwrap();
    f.until(|s| s["waiting"] == 1).await;
    f.harness.steer(&f.session, "background it").unwrap();
    assert!(f.registry.interrupt_waits(&f.session) > 0);
    f.harness.wait_for_idle(&f.session).await.unwrap();
    let execution = f.manager.status(&f.session)["environments"][0]["execution_id"].clone();
    f.cell(
        "monitor",
        "await tools.environments({action:'wake',environment:'main'});",
    )
    .await;
    f.until(|s| s["status"] == "idle").await;
    let result = f
        .cell(
            "monitor",
            "await tools.environments({action:'inspect',environment:'main'})",
        )
        .await;
    assert_eq!(result["result"], json!({"answer":"eventual"}));
    assert_eq!(result["result_execution_id"], execution);
    assert_eq!(result["state_changes"], json!(["state.finished added"]));
    assert!(f.texts().is_empty());
    f.manager.shutdown().await;
}

#[tokio::test]
async fn background_results_and_named_inspection_cannot_bypass_output_budget() {
    let f = Fixture::new();
    f.manager
        .control(
            &f.session,
            json!({"action":"create","environment":"monitor"}),
        )
        .unwrap();
    f.harness.start(&f.session,json!({"maxOutputTokens":256,"code":"await wait({ms:60000}); state.complete='x '.repeat(24000); return {body:state.complete};"}).to_string(),RunOptions::default()).unwrap();
    f.until(|s| s["waiting"] == 1).await;
    f.harness.steer(&f.session, "background it").unwrap();
    assert!(f.registry.interrupt_waits(&f.session) > 0);
    f.harness.wait_for_idle(&f.session).await.unwrap();
    assert_output_budget(&f.latest_tool_content(), 256);
    f.manager
        .control(&f.session, json!({"action":"wake","environment":"main"}))
        .unwrap();
    f.until(|s| s["status"] == "idle").await;
    let inspection = f
        .cell(
            "monitor",
            "await tools.environments({action:'inspect',environment:'main'})",
        )
        .await;
    assert_output_budget(&f.latest_tool_content(), 10000);
    assert_eq!(inspection["result"]["truncated"], true);
    let path = inspection["result"]["fullOutput"]["path"].as_str().unwrap();
    let full = std::fs::read_to_string(f._root.path().join(path)).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(full.split("\n[---]\n").next().unwrap()).unwrap()["body"],
        "x ".repeat(24000)
    );
    assert_eq!(f.cell("main", "state.complete.length").await, 48000);
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["generation"],
        1
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn failed_cell_retains_successful_command_incomplete_capture_without_raw_stdout() {
    let f = Fixture::with_native_tools();
    #[cfg(windows)]
    let script = "[Console]::Out.Write(('x' * 5461488))";
    #[cfg(not(windows))]
    let script = "head -c 5461488 /dev/zero | tr '\\000' x";
    let error = f.cell_error(&format!("const p=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:p.handle}}); const out=await tools.poll({{handle:p.handle}}); if(out.capture.stdout.gaps.length) throw Error('Could not reliably enumerate full tree');", serde_json::to_string(script).unwrap())).await;
    assert!(
        error.contains("Could not reliably enumerate full tree"),
        "{error}"
    );
    assert!(error.contains("Command succeeded (exit code 0)"), "{error}");
    assert!(error.contains("5461488 total bytes"), "{error}");
    assert!(error.contains("omitted bytes"), "{error}");
    assert!(error.contains("smaller output or batches"), "{error}");
    assert!(error.contains("file({path})"), "{error}");
    assert!(!error.contains(&"x".repeat(100)));
    assert!(error.len() < 2048);
    let inspect = f
        .manager
        .control(&f.session, json!({"action":"inspect","environment":"main"}))
        .unwrap();
    assert_eq!(inspect["last_error"], error);
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn failed_command_has_exit_stderr_and_can_be_corrected_without_stale_evidence() {
    let f = Fixture::with_native_tools();
    #[cfg(windows)]
    let bad = "[Console]::Error.Write('wrong option: --bogus'); exit 7";
    #[cfg(not(windows))]
    let bad = "printf 'wrong option: --bogus' >&2; exit 7";
    #[cfg(windows)]
    let good = "[Console]::Out.Write('corrected')";
    #[cfg(not(windows))]
    let good = "printf corrected";
    let error=f.cell_error(&format!("const p=await tools.exec({{command:{},yieldMs:0}}); const done=await wait({{handle:p.handle}}); if(done.exitCode!==0) throw Error('command attempt failed');",serde_json::to_string(bad).unwrap())).await;
    assert!(error.contains("code 7"), "{error}");
    assert!(error.contains("wrong option: --bogus"), "{error}");
    f.cell("main", &format!("const p=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:p.handle}}); text((await tools.poll({{handle:p.handle}})).stdout);",serde_json::to_string(good).unwrap())).await;
    assert_eq!(f.texts(), ["corrected"]);
    // Also clear earlier failure evidence within the same cell after a corrected command.
    let error=f.cell_error(&format!("const bad=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:bad.handle}}); const good=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:good.handle}}); throw Error('unrelated JS error');",serde_json::to_string(bad).unwrap(),serde_json::to_string(good).unwrap())).await;
    assert!(
        error.starts_with("unrelated JS error (line 1, column "),
        "{error}"
    );
    assert!(!error.contains("Last native command outcome"), "{error}");
    let schema = f.cell_error("await tools.exec({command:123});").await;
    assert!(schema.contains("exec"), "{schema}");
    assert!(schema.contains("/command"), "{schema}");
    assert!(
        schema.contains("string") || schema.contains("object"),
        "{schema}"
    );
    let launch = f
        .cell_error("await tools.exec({command:cmd`bubblepaw_missing_executable_92`});")
        .await;
    assert!(launch.contains("could not launch"), "{launch}");
    assert!(launch.contains("exec"), "{launch}");
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn removed_table_helper_is_unavailable_and_standard_outputs_still_work() {
    let f = Fixture::new();
    f.cell(
        "main",
        "text(typeof ui); const a=await file({path:'report.txt'}); text(a.name);",
    )
    .await;
    assert_eq!(f.texts(), ["undefined", "report.txt"]);
    assert!(
        f.store
            .outputs(&f.session, 20)
            .unwrap()
            .iter()
            .any(|r| r.output["type"] == "file")
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn cmd_tag_exec_roundtrips_native_argv_and_raw_multiline_remains_shell() {
    let f = Fixture::with_native_tools();
    let source = f._root.path().join("probe.rs");
    std::fs::write(
        &source,
        "fn main(){ println!(\"{:?}\", std::env::args().skip(1).collect::<Vec<_>>()); }",
    )
    .unwrap();
    let name = if cfg!(windows) {
        "argv_probe.exe"
    } else {
        "argv_probe"
    };
    let compiled = std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(f._root.path().join(name))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let args = [
        "space value",
        "O'Brien",
        "embedded\"quote",
        "$(effect)",
        "`effect`",
        "",
        "line\nnext",
        "雪🐾",
        "; echo effect > escaped.txt",
    ];
    let code = format!(
        r#"
      const args = {};
      const p = await tools.exec({{command:cmd`./{} ${{args[0]}} ${{args[1]}} ${{args[2]}} ${{args[3]}} ${{args[4]}} ${{args[5]}} ${{args[6]}} ${{args[7]}} ${{args[8]}}`,yieldMs:0}});
      await wait({{handle:p.handle,timeoutMs:5000}});
      const out = await tools.poll({{handle:p.handle}});
      text(out.stdout);
    "#,
        serde_json::to_string(&args).unwrap(),
        name
    );
    f.cell("main", &code).await;
    assert_eq!(f.texts(), [format!("{args:?}\n")]);
    assert!(!f._root.path().join("escaped.txt").exists());
    #[cfg(windows)]
    let raw = "[Console]::Out.Write('first');\n[Console]::Out.Write([char]10);\n[Console]::Out.Write('second')";
    #[cfg(not(windows))]
    let raw = "printf first\nprintf '\\n'\nprintf second";
    f.cell("main", &format!("const p=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:p.handle,timeoutMs:5000}}); text((await tools.poll({{handle:p.handle}})).stdout);",serde_json::to_string(raw).unwrap())).await;
    assert_eq!(f.texts()[1], "first\nsecond");
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn cmd_tag_preserves_raw_parts_primitive_values_and_rejects_objects() {
    let f = Fixture::new();
    f.cell(
        "main",
        r#"
      const query = "O'Brien\"$()`\n雪";
      const result = await tools.exec({command:cmd`rg --glob ${'*.rs'} -e ${query} ${0} ${false}`});
      text(JSON.stringify(result.command));
      const literal = await tools.exec({command:cmd`C:\native\probe.exe ${''}`});
      text(JSON.stringify(literal.command));
      for (const value of [null, undefined, {}, [], Infinity, NaN, () => 'script']) {
        try { cmd`probe ${value}`; text('unexpected acceptance'); }
        catch (e) { text(e.message); }
      }
    "#,
    )
    .await;
    let texts = f.texts();
    let structured: Value = serde_json::from_str(&texts[0]).unwrap();
    assert_eq!(
        structured,
        json!({"parts":["rg --glob "," -e "," "," ",""],"values":["*.rs","O'Brien\"$()`\n雪","0","false"]})
    );
    let literal: Value = serde_json::from_str(&texts[1]).unwrap();
    assert_eq!(
        literal,
        json!({"parts":[r"C:\native\probe.exe ",""],"values":[""]})
    );
    assert_eq!(texts.len(), 9);
    assert!(
        texts[2..]
            .iter()
            .all(|v| v.contains("cmd substitutions must be strings, finite numbers, or booleans"))
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn fresh_locals_persistent_state_async_dispatch_and_output_ownership() {
    let f = Fixture::new();
    let first = f.cell("main","const value = await tools.echo({value: 21}); state.count = value.value; state.oldText = text; text('first');").await;
    assert!(first["execution_id"].as_str().is_some());
    f.cell("main","const value = await tools.echo({value: state.count * 2}); state.count = value.value; try { state.oldText('wrong owner'); } catch(e) { text('old authority rejected'); } text(String(state.count));").await;
    assert_eq!(f.texts(), ["first", "old authority rejected", "42"]);
    let view = f.manager.status(&f.session);
    assert_eq!(view["environments"][0]["generation"], 1);
    assert_eq!(view["environments"][0]["state"][0]["preview"], 42);
    // Intermediate native results never become canonical provider tool messages.
    assert!(
        !f.store
            .messages(&f.session)
            .unwrap()
            .iter()
            .any(|m| m.content.contains("\"invocation\""))
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn named_environments_sessions_and_getter_free_inspection() {
    let f = Fixture::new();
    f.cell(
        "main",
        "await tools.environments({action:'create',environment:'diagnostics'});",
    )
    .await;
    f.cell("main","state.secret = 5; Object.defineProperty(state,'getter',{get(){text('getter ran');return 9;}}); state.object={toJSON(){text('toJSON ran');return 1;}}; Array.prototype.slice = () => {throw Error('patched slice');}; String.prototype.slice = () => {throw Error('patched string slice');};").await;
    f.cell(
        "diagnostics",
        "text(String(state.secret)); state.secret=10;",
    )
    .await;
    assert_eq!(f.texts(), ["undefined"]);
    let inspect = f
        .manager
        .control(&f.session, json!({"action":"inspect","environment":"main"}))
        .unwrap();
    assert_eq!(inspect["state"][1]["type"], "accessor");
    let other = f.store.create_session("other").unwrap();
    f.harness
        .prompt(
            &other.id,
            json!({"code":"text(String(state.secret)); state.secret=100;"}).to_string(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.store.outputs(&other.id, 20).unwrap()[0].output["text"],
        "undefined"
    );
    assert_eq!(
        f.manager.status(&f.session)["environments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        f.manager.status(&other.id)["environments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn javascript_controls_background_environment_and_validates_management() {
    let f = Fixture::new();
    f.cell("main", "await tools.environments({action:'create',environment:'monitor'}); state.count=7; for (const action of ['cancel','reset','close']) { try { await tools.environments({action,environment:'main',cancel:true}); throw Error('self control accepted'); } catch(e) { if(!e.message.includes('executing environment')) throw e; } } try { await tools.environments({action:'invented'}); throw Error('invalid action accepted'); } catch(e) { if(!e.message.includes('action')) throw e; } const info=await tools.environments({action:'inspect'}); if(info.environments.length!==2) throw Error('missing named environment');").await;
    f.harness
        .start(
            &f.session,
            json!({"code":"await wait({ms:60000}); text('resumed');"}).to_string(),
            RunOptions::default(),
        )
        .unwrap();
    f.until(|s| s["time_waiting"] == 1).await;
    f.harness.steer(&f.session, "new input").unwrap();
    assert!(f.registry.interrupt_waits(&f.session) > 0);
    f.harness.wait_for_idle(&f.session).await.unwrap();
    f.cell("monitor", "const info=await tools.environments({action:'inspect',environment:'main'}); if(info.status!=='background') throw Error('foreground still active'); await tools.environments({action:'wake',environment:'main'});").await;
    f.until(|s| s["status"] == "idle").await;
    assert_eq!(f.texts(), ["resumed"]);
    f.cell(
        "monitor",
        "await tools.environments({action:'reset',environment:'main'});",
    )
    .await;
    f.until(|s| s["generation"] == 2 && s["status"] == "idle")
        .await;
    f.cell("main", "if(state.count!==undefined) throw Error('reset retained state'); if(tools.js!==undefined) throw Error('recursive JS exposed');").await;
    f.manager.shutdown().await;
}

#[tokio::test]
async fn read_only_cells_keep_reads_waits_and_environment_management_without_mutations() {
    let f = Fixture::build(true, true);
    f.harness.prompt(&f.session,json!({"code":"if(['exec','edit','write','cancel','poll'].some(name=>name in tools)) throw Error('mutation exposed'); const info=await tools.read({path:'report.txt'}); await wait({ms:1}); await tools.environments({action:'create',environment:'readonly'}); text(info.content);"}).to_string(),RunOptions::default()).await.unwrap();
    let message = f
        .store
        .messages(&f.session)
        .unwrap()
        .into_iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(!message.is_error, "{}", message.content);
    assert_eq!(f.texts(), ["capture me"]);
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn invalid_native_arguments_can_be_corrected_in_a_later_cell() {
    let f = Fixture::with_native_tools();
    let error = f.cell_error("await tools.read({path:42});").await;
    assert!(
        error.contains("read") && error.contains("/path") && error.contains("string"),
        "{error}"
    );
    f.cell("main", "const report=await tools.read({path:'report.txt'}); const created=await tools.write({path:'corrected.txt',content:report.content+'\\n'}); text(created.path);").await;
    assert_eq!(f.texts(), ["corrected.txt"]);
    assert_eq!(
        std::fs::read_to_string(f._root.path().join("corrected.txt")).unwrap(),
        "capture me\n"
    );
    assert!(f.store.pending_effects(&f.session).unwrap().is_empty());
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn time_wait_background_continuation_and_real_wake() {
    let f = Fixture::new();
    f.harness.start(&f.session,json!({"code":"state.started=true; const result=await wait({ms:60000}); text(result.status); state.finished=true;"}).to_string(),RunOptions::default()).unwrap();
    f.until(|s| s["time_waiting"] == 1).await;
    assert!(
        f.manager
            .control(&f.session, json!({"action":"reset","environment":"main"}))
            .is_err()
    );
    f.harness.steer(&f.session, "new input").unwrap();
    assert!(f.registry.interrupt_waits(&f.session) > 0);
    f.harness.wait_for_idle(&f.session).await.unwrap();
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["status"],
        "background"
    );
    let id = f.manager.status(&f.session)["environments"][0]["execution_id"].clone();
    f.manager
        .control(&f.session, json!({"action":"wake","environment":"main"}))
        .unwrap();
    f.until(|s| s["status"] == "idle").await;
    assert_eq!(f.texts(), ["woken"]);
    let outputs = f.store.outputs(&f.session, 20).unwrap();
    assert!(
        outputs
            .iter()
            .all(|o| o.execution_id == id.as_str().unwrap())
    );
    assert!(
        outputs
            .iter()
            .any(|o| o.output["type"] == "execution" && o.output["status"] == "completed")
    );
    assert!(
        f.manager
            .control(&f.session, json!({"action":"wake","environment":"main"}))
            .is_err()
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn explicit_wait_does_not_spend_active_js_budget_and_cancel_recovers() {
    let f = Fixture::new();
    f.cell(
        "main",
        "await wait({ms:3200}); state.answer=42; text('elapsed');",
    )
    .await;
    f.harness
        .start(
            &f.session,
            json!({"code":"await wait({ms:60000}); text('should not finish');"}).to_string(),
            RunOptions::default(),
        )
        .unwrap();
    f.until(|s| s["waiting"] == 1).await;
    assert!(f.harness.cancel(&f.session));
    f.harness.wait_for_idle(&f.session).await.unwrap();
    f.until(|s| s["status"] == "idle").await;
    assert!(
        f.manager.status(&f.session)["environments"][0]["generation"]
            .as_u64()
            .unwrap()
            > 1
    );
    f.cell("main", "text('recovered');").await;
    assert!(
        f.latest_tool_content()
            .contains("state reset (previous failed/cancelled cell)")
    );
    assert_eq!(f.texts(), ["elapsed", "recovered"]);
    f.manager.shutdown().await;
}

#[tokio::test]
async fn infinite_jobs_are_bounded_even_with_pending_wait_and_memory_recovers() {
    let f = Fixture::new();
    for code in [
        "wait({ms:60000}); await Promise.resolve(); while(true){}",
        "state.huge = new Array(20000000).fill('allocation');",
    ] {
        tokio::time::timeout(
            Duration::from_secs(8),
            f.harness.prompt(
                &f.session,
                json!({"code":code}).to_string(),
                RunOptions::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let messages = f.store.messages(&f.session).unwrap();
        assert!(
            messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Tool)
                .unwrap()
                .is_error
        );
        f.until(|s| s["status"] == "idle").await;
    }
    f.cell("main", "text('usable after limits');").await;
    assert_eq!(f.texts(), ["usable after limits"]);
    f.manager.shutdown().await;
}

#[tokio::test]
async fn native_error_rejects_and_files_are_explicit_durable_outputs() {
    let f = Fixture::new();
    f.cell("main","try { await tools.reject({}); } catch(e) { text(e.message); } const artifact = await file({path:'report.txt',name:'download.txt'}); state.artifact=artifact; text('saved');").await;
    let outputs = f.store.outputs(&f.session, 20).unwrap();
    assert_eq!(f.texts(), ["deliberate rejection", "saved"]);
    let file = outputs.iter().find(|o| o.output["type"] == "file").unwrap();
    assert_eq!(file.output["artifact"]["name"], "download.txt");
    assert_eq!(file.output["artifact"]["size"], 10);
    assert!(file.output["artifact"].get("path").is_none());
    f.manager.shutdown().await;
}

#[tokio::test]
async fn unrelated_explicit_wait_cannot_hide_stalled_host_call_deadline() {
    let f = Fixture::new();
    tokio::time::timeout(
        Duration::from_secs(36),
        f.harness.prompt(
            &f.session,
            json!({"code":"wait({ms:60000}); await tools.stall({});"}).to_string(),
            RunOptions::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let messages = f.store.messages(&f.session).unwrap();
    let result = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(result.is_error);
    assert!(
        result.content.contains("Non-wait host call deadline"),
        "{}",
        result.content
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn native_file_mutations_are_discoverable_only_inside_js() {
    let f = Fixture::with_native_tools();
    let definitions = f.registry.definitions();
    assert_eq!(
        definitions
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["js"]
    );
    for name in ["edit", "write"] {
        assert!(f.registry.is_shown_always(name));
        assert!(
            definitions[0]
                .description
                .contains(&format!("tools.{name}("))
        );
    }
    let discovered = f.cell("main", "return {listed:await tools.$list(),selected:await tools.$search('select:edit,write')};").await;
    let mut names = discovered["listed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "cancel",
            "echo",
            "edit",
            "environments",
            "exec",
            "jobs",
            "list",
            "poll",
            "read",
            "reject",
            "search",
            "stall",
            "wait",
            "write"
        ]
    );
    assert!(
        discovered["selected"]["unknown"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for name in ["edit", "write"] {
        assert!(
            discovered["listed"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name && tool["shownAlways"] == true)
        );
        assert!(
            discovered["selected"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name)
        );
    }
    f.manager.shutdown().await;
}

#[tokio::test]
async fn native_edit_and_write_roundtrip_through_js_without_read_hashes() {
    let f = Fixture::with_native_tools();
    let content = "red cat\n\"quotes\" `backticks` ${literal} \\slash\n";
    let result = f.cell("main", &format!(
        "const created=await tools.write({}); const changed=await tools.edit({}); const replaced=await tools.edit({}); return {{created,changed,replaced}};",
        json!({"path":"other.txt","content":content}),
        json!({"path":"report.txt","old_string":"capture me","new_string":"keep me"}),
        json!({"path":"other.txt","old_string":"red cat","new_string":"blue dog"}),
    )).await;
    assert_eq!(result["created"]["path"], "other.txt");
    assert_eq!(result["created"]["bytes"], content.len());
    assert_eq!(result["created"]["created"], true);
    assert_eq!(result["changed"]["replacements"], 1);
    assert_eq!(result["replaced"]["replacements"], 1);
    assert_eq!(
        std::fs::read_to_string(f._root.path().join("report.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(
        std::fs::read_to_string(f._root.path().join("other.txt")).unwrap(),
        content.replace("red cat", "blue dog")
    );
    let failed = f.cell("main", &format!(
        "try {{ await tools.edit({}); throw Error('accepted invalid context'); }} catch(e) {{ return {{error:e.message}}; }}",
        json!({"path":"report.txt","old_string":"missing","new_string":"changed"})
    )).await;
    assert!(failed["error"].as_str().unwrap().contains("no exact match"));
    assert_eq!(
        std::fs::read_to_string(f._root.path().join("report.txt")).unwrap(),
        "keep me"
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn native_edit_waits_for_approval_and_denial_writes_nothing() {
    let f = Fixture::with_native_tools();
    let mut policy = ApprovalPolicy::default();
    policy.tools.insert("edit".into(), ApprovalMode::Ask);
    let code = format!(
        "try {{ return await tools.edit({}); }} catch(e) {{ return {{error:e.message}}; }}",
        json!({"path":"report.txt","old_string":"capture me","new_string":"keep me"})
    );
    for decision in [
        Decision::Deny {
            reason: Some("preserve this file".into()),
        },
        Decision::Approve,
    ] {
        let denied = matches!(decision, Decision::Deny { .. });
        let mut events = f.harness.subscribe();
        f.harness
            .start(
                &f.session,
                json!({"code":code}).to_string(),
                RunOptions {
                    approval: Some(policy.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        let approval = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let EventKind::ApprovalRequested { approval } = events.recv().await.unwrap().kind
                {
                    break approval;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(approval.tool, "edit");
        assert_eq!(
            std::fs::read_to_string(f._root.path().join("report.txt")).unwrap(),
            "capture me"
        );
        f.harness.decide(&approval.id, decision).unwrap();
        let run =
            tokio::time::timeout(Duration::from_secs(10), f.harness.wait_for_idle(&f.session))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(
            std::fs::read_to_string(f._root.path().join("report.txt")).unwrap(),
            if denied { "capture me" } else { "keep me" }
        );
    }
    f.manager.shutdown().await;
}

#[tokio::test]
async fn console_lines_are_cell_local_json_encoded_and_never_published() {
    let f = Fixture::new();
    let result = f.cell("main", "console.log('hello', {paw:1}, 42, null); console.info('info'); console.warn('warn'); console.error('error'); return {ok:true};").await;
    assert_eq!(result["result"], json!({"ok":true}));
    assert_eq!(
        result["logs"],
        json!(["hello {\"paw\":1} 42 null", "info", "warn", "error"])
    );
    assert_eq!(result["logs_omitted"], 0);
    assert!(f.texts().is_empty());
    assert_eq!(f.cell("main", "return 42;").await, 42);
    let result = f.cell("main", "console.log('receipt');").await;
    assert_eq!(result["logs"], json!(["receipt"]));
    let error = f
        .cell_error("console.log('before failure'); throw Error('broken');")
        .await;
    let error: Value = serde_json::from_str(error.split("\n[---]\n").next().unwrap()).unwrap();
    assert_eq!(error["logs"], json!(["before failure"]));
    assert!(
        error["result"]["error"]
            .as_str()
            .unwrap()
            .contains("broken")
    );
    assert!(f.texts().is_empty());
    for output in f.store.outputs(&f.session, 100).unwrap() {
        assert!(!output.output.to_string().contains("before failure"));
    }
    f.cell("main", "state.log = console.log; return 0;").await;
    let stale = f
        .cell(
            "main",
            "try { state.log('stale'); } catch(e) { return e.message; }",
        )
        .await;
    assert_eq!(stale, "Cell no longer owns output");
    f.manager.shutdown().await;
}

#[tokio::test]
async fn console_bounds_count_omitted_lines_and_share_the_reply_budget() {
    let f = Fixture::new();
    let result = f.cell_input(json!({"code":"for(let i=0;i<400;i++) console.log('line',i); return 'ok';", "maxOutputTokens":256})).await;
    let kept = result["logs"].as_array().unwrap().len();
    assert_eq!(
        kept + result["logs_omitted"].as_u64().unwrap() as usize,
        400
    );
    assert!(kept < 256);
    assert!(
        tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)
            .unwrap()
            .encode_ordinary(&f.latest_tool_content())
            .len()
            <= 256
    );
    let result = f
        .cell(
            "main",
            "console.log('x'.repeat(20000)); console.log('kept'); return 1;",
        )
        .await;
    assert_eq!(result["logs"], json!(["kept"]));
    assert_eq!(result["logs_omitted"], 1);
    let result = f.cell_input(json!({"code":"console.log('omitted with large return'); return 'x'.repeat(100000);", "maxOutputTokens":256})).await;
    assert_eq!(result["truncated"], true);
    assert_eq!(result["logs_omitted"], 1);
    assert!(result["logs"].as_array().unwrap().is_empty());
    assert!(f.texts().is_empty());
    f.manager.shutdown().await;
}

#[path = "codemode_runtime/error_locations.rs"]
mod error_locations;
