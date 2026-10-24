use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, EventKind, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider,
    Role, RunOptions, Store, TextCallback, ToolCall, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    agent_handles::register_agent_handles,
    agents::AgentTools,
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::Agents,
    net::{SearchBackend, WebConfig, register_fetch_search_tools_with_store},
    profiles::{AgentProfile, profile_harness},
    research::research_profile_with_tools,
    tools::WorkspaceTools,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    repl: Arc<CodeMode>,
    manager: Arc<AgentTools>,
    parent: Harness,
    session: String,
}
impl Fixture {
    fn new(provider: Arc<dyn Provider>, web: WebConfig) -> Self {
        Self::with_config(
            provider,
            web,
            HarnessConfig {
                model: "parent/model".into(),
                ..Default::default()
            },
            None,
            Duration::from_secs(30),
        )
    }
    fn with_config(
        provider: Arc<dyn Provider>,
        web: WebConfig,
        mut config: HarnessConfig,
        model: Option<String>,
        idle_limit: Duration,
    ) -> Self {
        config.execution_mode = bubblepaw_core::ExecutionMode::Js;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("notes.txt"),
            "Local fixture: cross-check fetched evidence.",
        )
        .unwrap();
        let store = Store::open(root.path().join("archive.sqlite")).unwrap();
        let repl = Arc::new(
            CodeMode::new(
                root.path().to_owned(),
                Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
            )
            .unwrap()
            .with_idle_limit(idle_limit),
        );
        let mut tools = ToolRegistry::new();
        WorkspaceTools::new(root.path())
            .unwrap()
            .register(&mut tools, false)
            .unwrap();
        tools.show_always(["read", "list", "search"]);
        let mut research = research_profile_with_tools(
            store.clone(),
            provider.clone(),
            &tools,
            &repl,
            &config,
            &web,
        )
        .unwrap();
        assert!(research.model.is_none());
        research.model = model;
        let read_only = AgentProfile {
            name: "read-only".into(),
            description: "Read-only fixture".into(),
            harness: profile_harness(
                store.clone(),
                provider.clone(),
                ToolRegistry::new(),
                &repl,
                config.clone(),
            )
            .unwrap(),
            model: None,
            max_rounds: Some(12),
            approval: None,
        };
        let manager = Arc::new(
            AgentTools::from_profiles(store.clone(), vec![read_only, research])
                .unwrap()
                .with_limits(Agents {
                    max_active_per_parent: Some(2),
                    ..Default::default()
                })
                .with_repl(repl.clone()),
        );
        manager.register(&mut tools).unwrap();
        register_agent_handles(&mut tools, manager.clone()).unwrap();
        register_fetch_search_tools_with_store(&mut tools, &web, &store).unwrap();
        assert!(tools.get_host("research").is_none());
        repl.register(&mut tools).unwrap();
        let parent = Harness::new(store.clone(), provider, tools, config);
        let session = store.create_session("parent").unwrap().id;
        Self {
            _root: root,
            store,
            repl,
            manager,
            parent,
            session,
        }
    }
    async fn script(&self, code: &str) -> Value {
        let run = self
            .parent
            .prompt(
                &self.session,
                "research fixture",
                RunOptions {
                    script: Some(code.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(run.status, "completed", "{:?}", run.error);
        serde_json::from_str(run.text.as_deref().unwrap()).unwrap()
    }
    fn context(&self) -> ToolContext {
        ToolContext {
            session_id: self.session.clone(),
            run_id: "parent-run".into(),
            invocation_id: uuid::Uuid::new_v4().to_string(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        }
    }
    async fn shutdown(&self) {
        self.manager.shutdown().await.unwrap();
        self.repl.shutdown().await;
    }
}
fn js(code: &str) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: "js".into(),
            arguments: json!({"code":code}),
        }],
        ..Default::default()
    }
}
#[derive(Default)]
struct ResearchConversation {
    requests: Mutex<Vec<ModelRequest>>,
    parent_round: AtomicUsize,
    child_round: AtomicUsize,
    source: Mutex<Option<String>>,
}
#[async_trait]
impl Provider for ResearchConversation {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "js");
        let child = request
            .system_prompt
            .starts_with("Answer the assigned question using web_search, web_fetch");
        self.requests.lock().unwrap().push(request.clone());
        if child {
            assert!(
                !request
                    .messages
                    .iter()
                    .any(|m| m.content.contains("PARENT PRIVATE"))
            );
            let round = self.child_round.fetch_add(1, Ordering::SeqCst);
            return match round {
                0 => Ok(js(
                    "for(const name of ['agent','agent_send','research','write','edit','exec']) if(name in tools) throw Error('unexpected research capability '+name); for(const name of ['read','list','search','web_search','web_fetch']) if(!(name in tools) || typeof tools[name]!=='function') throw Error('missing research capability '+name); const notes=await tools.read({path:'notes.txt'}); if(!notes.content.includes('cross-check')) throw Error('missing file read'); const hits=await tools.web_search({query:'fixture evidence'}); state.source=hits.results[0].id; const page=await tools.web_fetch({id:state.source}); if(page.id!==state.source || !page.content.includes('supports the answer')) throw Error('missing fetched evidence'); return {id:page.id,title:page.title,notes:notes.content};",
                )),
                1 | 3 => {
                    let result = request
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == Role::Tool)
                        .unwrap();
                    assert!(!result.is_error, "{}", result.content);
                    let value: Value =
                        serde_json::from_str(result.content.split("\n[---]\n").next().unwrap())?;
                    let id = value["id"].as_str().unwrap().to_owned();
                    if round == 1 {
                        *self.source.lock().unwrap() = Some(id.clone());
                    } else {
                        assert_eq!(self.source.lock().unwrap().as_deref(), Some(id.as_str()));
                        assert!(
                            request.messages.iter().any(|m| m.role == Role::Assistant
                                && m.content.contains(&format!("[{id}]")))
                        );
                    }
                    Ok(ModelResponse {
                        text: format!(
                            "{} [{id}].",
                            if round == 1 {
                                "The fixture supports the answer"
                            } else {
                                "The same source confirms the followup"
                            }
                        ),
                        ..Default::default()
                    })
                }
                2 => {
                    assert!(request.messages.iter().any(|m| m.role == Role::User
                        && m.content == "What exactly does the same source confirm?"));
                    let id = self.source.lock().unwrap().clone().unwrap();
                    assert!(request.messages.iter().any(
                        |m| m.role == Role::Assistant && m.content.contains(&format!("[{id}]"))
                    ));
                    let literal = serde_json::to_string(&id)?;
                    Ok(js(&format!(
                        "const id={literal}; const page=await tools.web_fetch({{id,query:'supports the answer',top_k:1}}); if(page.id!==id || page.passages.length!==1 || !page.passages[0].text.includes('supports the answer')) throw Error('missing selected passage'); return {{id:page.id,passage:page.passages[0]}};",
                    )))
                }
                _ => bail!("unexpected child round {round}"),
            };
        }
        match self.parent_round.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(js(
                "if('research' in tools) throw Error('dedicated research tool remains'); state.worker=await tools.agent({profile:'research',task:'What does the fixture show?',context:{purpose:'offline'}}); const first=await wait({handle:state.worker.handle}); if(first.status!=='completed' || !first.text.includes('[s')) throw Error(JSON.stringify(first)); return {sessionId:state.worker.sessionId,answer:first.text};",
            )),
            1 => Ok(js(
                "const sent=await tools.agent_send({sessionId:state.worker.sessionId,text:'What exactly does the same source confirm?'}); if(sent.sessionId!==state.worker.sessionId) throw Error('followup created a different child'); const second=await wait({sessionId:state.worker.sessionId}); if(second.status!=='completed' || !second.text.includes('same source confirms')) throw Error(JSON.stringify(second)); return {sessionId:state.worker.sessionId,answer:second.text};",
            )),
            2 => {
                let id = self.source.lock().unwrap().clone().unwrap();
                Ok(js(&format!(
                    "const source=await tools.web_fetch({{id:{}}}); if(!source.content.includes('supports the answer')) throw Error('parent cannot open child source'); const hits=await tools.web_search({{query:'fixture evidence'}}); if(hits.results[0].id!==source.id) throw Error('root source identity changed'); return {{id:source.id,content:source.content}};",
                    serde_json::to_string(&id)?
                )))
            }
            3 => {
                assert!(
                    request
                        .messages
                        .iter()
                        .filter(|m| m.role == Role::Tool)
                        .all(|m| !m.is_error)
                );
                Ok(ModelResponse {
                    text: format!(
                        "Research and followup completed [{}].",
                        self.source.lock().unwrap().as_deref().unwrap()
                    ),
                    ..Default::default()
                })
            }
            round => bail!("unexpected parent round {round}"),
        }
    }
}
/// Scripted model decisions and loopback HTTP only; no live model/web service.
#[tokio::test]
async fn main_model_uses_ordinary_research_child_and_follows_up_in_its_persisted_session() {
    use axum::{Json, Router, response::Html, routing::get};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let page = format!("{base}/page");
    let result_url = page.clone();
    let search_calls = Arc::new(AtomicUsize::new(0));
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let searches = search_calls.clone();
    let fetches = fetch_calls.clone();
    let app = Router::new()
        .route("/search", get(move || {
            let url = result_url.clone();
            searches.fetch_add(1, Ordering::SeqCst);
            async move { Json(json!({"results":[{"title":"Fixture title","url":url,"content":"Synthetic supporting evidence"}]})) }
        }))
        .route("/page", get(move || {
            fetches.fetch_add(1, Ordering::SeqCst);
            async { Html("<html><head><title>Fixture title</title></head><body><h1>Evidence</h1><p>The fixture supports the answer.</p></body></html>") }
        }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let provider = Arc::new(ResearchConversation::default());
    let web = WebConfig {
        search: vec![SearchBackend::SearxNg { url: base }],
        allow_private: true,
        ..Default::default()
    };
    let fixture = Fixture::new(provider.clone(), web.clone());
    let run = fixture
        .parent
        .prompt(
            &fixture.session,
            "Investigate the fixture. PARENT PRIVATE context stays here.",
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let source = provider.source.lock().unwrap().clone().unwrap();
    assert_eq!(
        run.text.as_deref(),
        Some(format!("Research and followup completed [{source}].").as_str())
    );
    assert_eq!(provider.parent_round.load(Ordering::SeqCst), 4);
    assert_eq!(provider.child_round.load(Ordering::SeqCst), 4);
    assert_eq!(
        search_calls.load(Ordering::SeqCst),
        1,
        "root sessions share the cached search"
    );
    assert_eq!(
        fetch_calls.load(Ordering::SeqCst),
        1,
        "followup and parent read cached evidence"
    );
    let sessions = fixture.store.list_sessions().unwrap();
    assert_eq!(sessions.len(), 2);
    let child = sessions.iter().find(|s| s.id != fixture.session).unwrap();
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.session_id == fixture.session)
                .count(),
            4
        );
        assert_eq!(
            requests.iter().filter(|r| r.session_id == child.id).count(),
            4
        );
        assert!(requests.iter().all(|r| r.model == "parent/model"));
    }
    assert_eq!(
        child.parent_session_id.as_deref(),
        Some(fixture.session.as_str())
    );
    assert!(!child.read_only);
    let listing = fixture
        .manager
        .status(&fixture.session, &json!({}))
        .unwrap();
    assert_eq!(listing["children"][0]["sessionId"], child.id);
    assert_eq!(
        fixture
            .manager
            .status(&fixture.session, &json!({"sessionId":child.id}))
            .unwrap()["status"],
        "idle"
    );
    let messages = fixture.store.messages(&child.id).unwrap();
    assert_eq!(messages.iter().filter(|m| m.role == Role::User).count(), 2);
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.role == Role::Assistant && m.content.contains(&format!("[{source}]")))
            .count(),
        2
    );
    assert!(
        messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .all(|m| !m.is_error)
    );
    assert!(
        !messages
            .iter()
            .any(|m| m.content.contains("PARENT PRIVATE"))
    );
    assert_eq!(fixture.store.runs(&child.id).unwrap().len(), 2);
    assert!(
        fixture
            .store
            .runs(&child.id)
            .unwrap()
            .iter()
            .all(|r| r.status == "completed")
    );
    fixture.shutdown().await;
    let Fixture {
        _root: root,
        store,
        repl,
        manager,
        parent,
        session,
    } = fixture;
    drop((parent, manager, repl, store));
    let reopened = Store::open(root.path().join("archive.sqlite")).unwrap();
    assert_eq!(
        reopened.session(&child.id).unwrap().parent_session_id,
        Some(session.clone())
    );
    assert_eq!(reopened.messages(&child.id).unwrap(), messages);
    assert_eq!(reopened.runs(&child.id).unwrap().len(), 2);
    let repl = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    let mut reopened_tools = ToolRegistry::new();
    WorkspaceTools::new(root.path())
        .unwrap()
        .register(&mut reopened_tools, true)
        .unwrap();
    let profile = research_profile_with_tools(
        reopened.clone(),
        provider,
        &reopened_tools,
        &repl,
        &HarnessConfig {
            model: "parent/model".into(),
            ..Default::default()
        },
        &web,
    )
    .unwrap();
    let restarted = AgentTools::from_profiles(reopened.clone(), vec![profile]).unwrap();
    assert_eq!(
        restarted.status(&session, &json!({})).unwrap()["children"][0]["sessionId"],
        child.id
    );
    assert_eq!(
        restarted
            .status(&session, &json!({"sessionId":child.id}))
            .unwrap()["state"],
        "idle"
    );
    register_fetch_search_tools_with_store(&mut reopened_tools, &web, &reopened).unwrap();
    let context = ToolContext {
        session_id: session,
        run_id: "reopened-run".into(),
        invocation_id: uuid::Uuid::new_v4().to_string(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let fetched = reopened_tools
        .get_host("web_fetch")
        .unwrap()
        .invoke(context.clone(), json!({"id":source}))
        .await
        .unwrap();
    assert!(!fetched.is_error, "{}", fetched.content);
    let fetched: Value = serde_json::from_str(&fetched.content).unwrap();
    assert_eq!(fetched["id"], source);
    assert_eq!(fetched["source"]["url"], page);
    assert_eq!(fetched["source"]["title"], "Fixture title");
    assert!(
        fetched["content"]
            .as_str()
            .unwrap()
            .contains("supports the answer")
    );
    let searched = reopened_tools
        .get_host("web_search")
        .unwrap()
        .invoke(context, json!({"query":"fixture evidence"}))
        .await
        .unwrap();
    assert!(!searched.is_error, "{}", searched.content);
    let searched: Value = serde_json::from_str(&searched.content).unwrap();
    assert_eq!(searched["results"][0]["id"], source);
    assert_eq!(searched["results"][0]["url"], page);
    assert_eq!(search_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 1);
    restarted.shutdown().await.unwrap();
    repl.shutdown().await;
    server.abort();
}
#[derive(Default)]
struct Probe {
    requests: Mutex<Vec<ModelRequest>>,
    active: AtomicUsize,
    entered: Notify,
    release: Notify,
    block: bool,
    loop_tools: bool,
    parent_research: bool,
}
#[async_trait]
impl Provider for Probe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let start_parent_research = self.parent_research
            && !request
                .system_prompt
                .starts_with("Answer the assigned question using web_search, web_fetch")
            && !request
                .messages
                .iter()
                .any(|message| message.role == Role::Tool);
        self.requests.lock().unwrap().push(request);
        if start_parent_research {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":"const child=await tools.agent({task:'Inherit the actual parent model',profile:'research'}); return await wait({handle:child.handle});"}),
                }],
                ..Default::default()
            });
        }
        if self.block {
            self.active.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_waiters();
            tokio::select! { _ = cancellation.cancelled() => bail!("cancelled fixture"), _ = self.release.notified() => {} }
        }
        if self.loop_tools {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":"return 1;"}),
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "Research answer.\nSources:\n- [Page](https://example.com): A quote".into(),
            ..Default::default()
        })
    }
}
async fn wait_active(probe: &Probe, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while probe.active.load(Ordering::SeqCst) < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn research_profile_model_override_selection_and_ordinary_aliases() {
    let probe = Arc::new(Probe::default());
    let fixture = Fixture::with_config(
        probe.clone(),
        WebConfig::default(),
        HarnessConfig {
            model: "parent/model".into(),
            ..Default::default()
        },
        Some("research-provider/model".into()),
        Duration::from_secs(30),
    );
    let result = fixture
        .script("const child=await tools.agent({task:'Check configured model',profile:'research'}); return await wait({handle:child.handle});")
        .await;
    assert_eq!(result["status"], "completed");
    assert_eq!(
        probe.requests.lock().unwrap()[0].model,
        "research-provider/model"
    );
    let child = fixture
        .script("return await tools.agent({task:'agent profile research',profile:'research'});")
        .await;
    assert_eq!(
        fixture
            .manager
            .wait(&fixture.session, child["handle"].as_str().unwrap(), None)
            .await
            .unwrap()["status"],
        "completed"
    );
    assert_eq!(
        probe.requests.lock().unwrap()[1].model,
        "research-provider/model"
    );
    let mut registry = ToolRegistry::new();
    fixture.manager.register(&mut registry).unwrap();
    let schema = registry
        .get_host("agent")
        .unwrap()
        .definition()
        .input_schema;
    assert_eq!(
        schema["properties"]["profile"]["enum"],
        json!(["read-only", "research"])
    );
    assert_eq!(schema["properties"]["access"]["deprecated"], true);
    assert!(
        fixture
            .manager
            .start(
                &fixture.context(),
                json!({"task":"conflict","profile":"research","access":"read-only"})
            )
            .is_err()
    );
    let alias = fixture
        .manager
        .start(
            &fixture.context(),
            json!({"task":"old alias","access":"read-only"}),
        )
        .unwrap();
    assert_eq!(
        fixture
            .manager
            .wait(&fixture.session, alias["handle"].as_str().unwrap(), None)
            .await
            .unwrap()["status"],
        "completed"
    );
    assert_eq!(probe.requests.lock().unwrap()[2].model, "parent/model");
    fixture.shutdown().await;
}
#[tokio::test]
async fn research_without_model_override_inherits_the_parent_run_model() {
    let probe = Arc::new(Probe {
        parent_research: true,
        ..Default::default()
    });
    let fixture = Fixture::new(probe.clone(), WebConfig::default());
    let run = fixture
        .parent
        .prompt(
            &fixture.session,
            "research with a per-run model",
            RunOptions {
                model: Some("provider/per-run-override".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let request = probe
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| {
            request
                .system_prompt
                .starts_with("Answer the assigned question using web_search, web_fetch")
        })
        .unwrap()
        .clone();
    assert_eq!(request.model, "provider/per-run-override");
    fixture.shutdown().await;
}
#[tokio::test]
async fn concurrent_research_profiles_share_the_ordinary_parent_child_limit() {
    let probe = Arc::new(Probe {
        block: true,
        ..Default::default()
    });
    let fixture = Fixture::new(probe.clone(), WebConfig::default());
    let mut events = fixture.parent.subscribe();
    fixture.parent.start(&fixture.session, "parallel research", RunOptions { script: Some("const results=await Promise.allSettled([1,2,3].map(async n=>{const child=await tools.agent({task:'Question '+n,profile:'research'});return await wait({handle:child.handle});})); return results.map(r=>r.status==='fulfilled'?r:{status:r.status,reason:String(r.reason)});".into()), ..Default::default() }).unwrap();
    wait_active(&probe, 2).await;
    // Observe the actual capacity rejection before releasing the active children.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EventKind::ToolFinished { name, result, .. } = events.recv().await.unwrap().kind
                && name == "agent"
                && result.is_error
            {
                assert!(result.content.contains("parent agent concurrency limit"));
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(probe.active.load(Ordering::SeqCst), 2);
    probe.release.notify_waiters();
    let run = fixture
        .parent
        .wait_for_idle(&fixture.session)
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let result: Value = serde_json::from_str(run.text.as_deref().unwrap()).unwrap();
    assert_eq!(
        result
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["status"] == "fulfilled")
            .count(),
        2
    );
    assert_eq!(
        result
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["status"] == "rejected")
            .count(),
        1
    );
    assert_eq!(fixture.store.list_sessions().unwrap().len(), 3);
    fixture.shutdown().await;
}

#[tokio::test]
async fn parent_cancellation_cancels_the_research_child() {
    let probe = Arc::new(Probe {
        block: true,
        ..Default::default()
    });
    let fixture = Fixture::new(probe.clone(), WebConfig::default());
    fixture
        .parent
        .start(
            &fixture.session,
            "cancel research",
            RunOptions {
                script: Some(
                    "const child=await tools.agent({task:'Wait for cancellation',profile:'research'}); return await wait({handle:child.handle});".into(),
                ),
                ..Default::default()
            },
        )
        .unwrap();
    wait_active(&probe, 1).await;
    fixture.parent.cancel(&fixture.session);
    assert_eq!(
        fixture
            .parent
            .wait_for_idle(&fixture.session)
            .await
            .unwrap()
            .status,
        "cancelled"
    );
    let child = fixture
        .store
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.id != fixture.session)
        .unwrap();
    let run = tokio::time::timeout(
        Duration::from_secs(5),
        fixture
            .manager
            .profile("research")
            .unwrap()
            .harness
            .wait_for_idle(&child.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(run.status, "cancelled");
    assert_eq!(
        fixture.store.runs(&child.id).unwrap()[0].status,
        "cancelled"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn ordinary_research_profile_uses_the_harness_round_limit() {
    let probe = Arc::new(Probe {
        loop_tools: true,
        ..Default::default()
    });
    let fixture = Fixture::with_config(
        probe.clone(),
        WebConfig::default(),
        HarnessConfig {
            max_rounds: Some(2),
            ..Default::default()
        },
        None,
        Duration::from_secs(30),
    );
    assert_eq!(
        fixture.manager.profile("research").unwrap().max_rounds,
        Some(2)
    );
    assert_eq!(
        fixture
            .script("const child=await tools.agent({task:'bounded research loop',profile:'research'}); return await wait({handle:child.handle});")
            .await["status"],
        "failed"
    );
    assert_eq!(probe.requests.lock().unwrap().len(), 2);
    fixture.shutdown().await;
}

#[path = "research/idle_deadline.rs"]
mod idle_deadline;
#[path = "research/profile_policies.rs"]
mod profile_policies;
