//! Scripted model-facing acceptance: no provider transport or live model calls.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider,
    Role, RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition,
    ToolRegistry, ToolResult,
};
use bubblepaw_runner::{
    agent_handles::register_agent_handles, agents::AgentTools, artifacts::ArtifactStore,
    codemode::CodeMode,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct CountingRead(Arc<AtomicUsize>);

#[async_trait]
impl Tool for CountingRead {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: "Read a synthetic fixture, recording actual invocations.".into(),
            input_schema: json!({
                "type":"object", "properties":{"path":{"type":"string"}},
                "required":["path"], "additionalProperties":false
            }),
            read_only: true,
            approval: Approval::None,
        }
    }

    async fn invoke(&self, _: ToolContext, input: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text(
            json!({"content":input["path"]}).to_string(),
        ))
    }
}

#[derive(Default)]
struct ControlProvider {
    requests: Mutex<Vec<ModelRequest>>,
}

fn js_response(code: &str) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: "js".into(),
            arguments: json!({"code":code}),
        }],
        ..Default::default()
    }
}

#[async_trait]
impl Provider for ControlProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let (index, user) = request
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| {
                message.role == Role::User
                    && message.origin != Some(bubblepaw_core::MessageOrigin::Generated)
            })
            .unwrap();
        let input = user.content.clone();
        let answered = request.messages[index + 1..]
            .iter()
            .any(|message| message.role == Role::Tool);
        self.requests.lock().unwrap().push(request.clone());
        if let Some(code) = input.strip_prefix("PARENT CELL:") {
            if !answered {
                assert_eq!(request.tools.len(), 1);
                let api = &request.tools[0].description;
                assert!(api.contains("tools.agent("), "{api}");
                for field in ["output_schema?", "max_tool_calls?", "timeout?"] {
                    assert!(api.contains(field), "missing {field}: {api}");
                }
                return Ok(js_response(code));
            }
            return Ok(ModelResponse {
                text: "parent finished".into(),
                ..Default::default()
            });
        }
        let step = if input.contains("control:first") {
            1
        } else if input.contains("control:second") {
            2
        } else if input.contains("control:third") {
            3
        } else if input.contains("control:clear") {
            assert!(request.output_schema.is_none());
            // Clearing the run contract preserves the chat's initial JS help.
            // The current-run notice revokes completion even while that help remains.
            assert!(
                request.tools[0]
                    .description
                    .contains("tools.submit_result({step})")
            );
            assert!(request.messages.iter().any(|message| {
                message
                    .content
                    .contains("This run has no output schema. submit_result is unavailable")
            }));
            assert_eq!(request.tools.len(), 1, "the zero-budget round keeps tools");
            return Ok(ModelResponse {
                text: "ordinary answer after clearing schema".into(),
                ..Default::default()
            });
        } else {
            assert!(input.contains("control:batch"), "unexpected input: {input}");
            if !answered {
                assert_eq!(request.tools.len(), 1);
                return Ok(js_response(
                    "return await Promise.all(['a','b','c','d'].map(async path=>{try{return {ok:true,...await tools.read({path})};}catch(error){return {ok:false,error:String(error)};}}));",
                ));
            }
            assert!(
                !request.tools.is_empty(),
                "exhausted round keeps its declarations"
            );
            let result = request.messages[index + 1..]
                .iter()
                .find(|message| message.role == Role::Tool)
                .unwrap();
            assert!(
                result.content.ends_with("0 tool calls left"),
                "{}",
                result.content
            );
            let batch: Value = serde_json::from_str(
                result
                    .content
                    .split("\n\n0 tool calls left")
                    .next()
                    .unwrap(),
            )?;
            let batch = batch.as_array().unwrap();
            assert_eq!(batch.iter().filter(|value| value["ok"] == true).count(), 2);
            let rejected: Vec<_> = batch.iter().filter(|value| value["ok"] == false).collect();
            assert_eq!(rejected.len(), 2);
            for value in rejected {
                assert!(
                    value["error"]
                        .as_str()
                        .unwrap()
                        .contains("budget exhausted")
                );
            }
            return Ok(ModelResponse {
                text: "answer from two permitted reads".into(),
                ..Default::default()
            });
        };
        assert!(request.output_schema.is_none());
        assert!(
            request.tools[0]
                .description
                .contains("tools.submit_result({step})")
        );
        if step <= 2 {
            let budget = if step == 1 { 0 } else { 2 };
            assert!(input.contains(&format!("You have {budget} tool calls")));
        }
        if step > 1 {
            assert!(
                request.messages.iter().any(|message| {
                    message.role == Role::Assistant && message.content == "{\"step\":1}"
                }),
                "follow-up must retain the first complete answer"
            );
        }
        // The result remains reachable when ordinary tool calls are exhausted.
        assert_eq!(request.tools.len(), 1, "tools are declared on every round");
        if step > 1 && !answered {
            return Ok(js_response(
                "return await tools.read({path:'followup evidence'});",
            ));
        }
        Ok(js_response(&format!(
            "return await tools.submit_result({});",
            json!({"step":step})
        )))
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    parent_id: String,
    harness: Harness,
    agents: Arc<AgentTools>,
    repl: Arc<CodeMode>,
    reads: Arc<AtomicUsize>,
    provider: Arc<ControlProvider>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let store = Store::in_memory()?;
        let parent_id = store.create_session("model-facing child controls")?.id;
        let provider = Arc::new(ControlProvider::default());
        let reads = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools.show_always(bubblepaw_runner::tools::ALWAYS_SHOWN_TOOLS.iter().copied());
        tools.register_host(Arc::new(CountingRead(reads.clone())))?;
        let repl = Arc::new(CodeMode::new(
            directory.path().to_owned(),
            Arc::new(ArtifactStore::new(
                directory.path(),
                directory.path().join("artifacts"),
            )?),
        )?);
        let mut child_tools = tools.read_only();
        repl.register(&mut child_tools)?;
        let child = Harness::new(
            store.clone(),
            provider.clone(),
            child_tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        let agents = Arc::new(AgentTools::new(store.clone(), child, None).with_repl(repl.clone()));
        agents.register(&mut tools)?;
        register_agent_handles(&mut tools, agents.clone())?;
        repl.register(&mut tools)?;
        let harness = Harness::new(
            store.clone(),
            provider.clone(),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Ok(Self {
            _directory: directory,
            store,
            parent_id,
            harness,
            agents,
            repl,
            reads,
            provider,
        })
    }

    async fn cell(&self, code: &str) -> Result<Value> {
        let run = tokio::time::timeout(
            Duration::from_secs(10),
            self.harness.prompt(
                &self.parent_id,
                format!("PARENT CELL:{code}"),
                RunOptions::default(),
            ),
        )
        .await??;
        assert_eq!(run.status, "completed");
        let messages = self.store.messages(&self.parent_id)?;
        let reply = messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(!reply.is_error, "{}", reply.content);
        assert!(self.store.pending_effects(&self.parent_id)?.is_empty());
        Ok(serde_json::from_str(&reply.content)?)
    }

    async fn close(&self) -> Result<()> {
        self.agents.shutdown().await?;
        self.repl.shutdown().await;
        Ok(())
    }
}

#[tokio::test]
async fn js_child_controls_validate_json_and_followups_keep_history_with_fresh_limits() -> Result<()>
{
    let fixture = Fixture::new()?;
    let result = fixture.cell(
        "const child=await tools.agent({task:'control:first',output_schema:{name:'child_answer',schema:{type:'object',properties:{step:{type:'integer'}},required:['step'],additionalProperties:false}},max_tool_calls:0,timeout:10000}); const first=await wait({handle:child.handle}); await tools.agent_send({handle:child.handle,text:'control:second',max_tool_calls:2,timeout:10000}); const second=await wait({handle:child.handle}); await tools.agent_send({sessionId:child.sessionId,text:'control:third'}); const third=await wait({sessionId:child.sessionId}); await tools.agent_send({sessionId:child.sessionId,text:'control:clear',output_schema:null,max_tool_calls:0}); const cleared=await wait({sessionId:child.sessionId}); return {first,second,third,cleared};",
    ).await?;
    for (key, step) in [("first", 1), ("second", 2), ("third", 3)] {
        assert_eq!(result[key]["status"], "completed");
        assert_eq!(result[key]["json"], json!({"step":step}));
        assert_eq!(result[key]["sessionId"], result["first"]["sessionId"]);
    }
    assert_eq!(result["cleared"]["status"], "completed");
    assert_eq!(
        result["cleared"]["text"],
        "ordinary answer after clearing schema"
    );
    assert!(result["cleared"].get("json").is_none_or(Value::is_null));
    assert_eq!(fixture.reads.load(Ordering::SeqCst), 2);
    let child_id = result["first"]["sessionId"].as_str().unwrap();
    assert_eq!(fixture.store.runs(child_id)?.len(), 4);
    {
        let requests = fixture.provider.requests.lock().unwrap();
        let child_requests = requests
            .iter()
            .filter(|request| request.session_id == child_id)
            .collect::<Vec<_>>();
        let initial = child_requests[0];
        let frozen = serde_json::to_value(&initial.tools)?;
        for request in child_requests {
            assert_eq!(serde_json::to_value(&request.tools)?, frozen);
            assert_eq!(request.system_prompt, initial.system_prompt);
        }
    }
    fixture.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_js_parallel_native_calls_share_budget_and_reject_before_invocation() -> Result<()> {
    let fixture = Fixture::new()?;
    let result = fixture.cell(
        "const child=await tools.agent({task:'control:batch',max_tool_calls:3,timeout:10000}); return await wait({handle:child.handle});",
    ).await?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["text"], "answer from two permitted reads");
    assert_eq!(
        fixture.reads.load(Ordering::SeqCst),
        2,
        "outer js consumes one of three slots"
    );
    {
        let requests = fixture.provider.requests.lock().unwrap();
        let child_requests: Vec<_> = requests
            .iter()
            .filter(|request| request.session_id != fixture.parent_id)
            .collect();
        assert_eq!(child_requests.len(), 2);
        assert!(
            child_requests[0]
                .messages
                .iter()
                .any(|message| message.content.contains("You have 3 tool calls"))
        );
        // The exhausted round keeps the declarations; only the reminder asks for text.
        assert!(!child_requests[1].tools.is_empty());
        assert!(child_requests[1].disabled_tools.is_empty());
        assert_eq!(
            serde_json::to_value(&child_requests[1].tools)?,
            serde_json::to_value(&child_requests[0].tools)?
        );
    }
    fixture.close().await
}
