//! Scripted live child settings and registry refresh; no network/model calls.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, ApprovalMode, ApprovalPolicy, CancellationToken, Harness, HarnessConfig,
    ModelRequest, ModelResponse, Provider, Role, RunOptions, Store, TextCallback, Tool, ToolCall,
    ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use bubblepaw_runner::{
    agent_handles::register_agent_handles,
    agents::{AgentRegistry, AgentTools},
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::Agents,
    profiles::child_agent_host,
    tools::register_native_tools,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

struct Probe {
    requests: Mutex<Vec<ModelRequest>>,
    entered: mpsc::UnboundedSender<String>,
    release: Notify,
}
#[async_trait]
impl Provider for Probe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancel: CancellationToken,
    ) -> Result<ModelResponse> {
        let (index, input) = request
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| {
                message.role == Role::User
                    && message.origin != Some(bubblepaw_core::MessageOrigin::Generated)
            })
            .unwrap();
        let effect = input.content == "gated effect";
        let list_inventory = input.content == "next";
        let finished = request.messages[index + 1..]
            .iter()
            .any(|message| message.role == Role::Tool);
        let session = request.session_id.clone();
        self.requests.lock().unwrap().push(request);
        if list_inventory && !finished {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":"return await tools.$list();"}),
                }],
                ..Default::default()
            });
        }
        if effect && !finished {
            self.entered.send(session)?;
            tokio::select! {
                _ = self.release.notified() => {},
                _ = cancel.cancelled() => anyhow::bail!("cancelled"),
            }
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":"return await tools.effect({});"}),
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}
struct Capability {
    name: &'static str,
    read_only: bool,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for Capability {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: format!("Fixture {}", self.name),
            input_schema: json!({"type":"object"}),
            read_only: self.read_only,
            approval: if self.name == "effect" {
                Approval::Confirm
            } else {
                Approval::None
            },
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("called"))
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    parent: Harness,
    manager: Arc<AgentTools>,
    tools: Arc<RwLock<ToolRegistry>>,
    policy: Arc<RwLock<ApprovalPolicy>>,
    probe: Arc<Probe>,
    entered: mpsc::UnboundedReceiver<String>,
    effects: Arc<AtomicUsize>,
    owner: ToolContext,
    _repl: Arc<CodeMode>,
}
fn allow() -> ApprovalPolicy {
    ApprovalPolicy {
        confirm: ApprovalMode::Allow,
        ..Default::default()
    }
}
impl Fixture {
    fn new(depth: Option<usize>) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let root = store.create_session("parent")?;
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let probe = Arc::new(Probe {
            requests: Mutex::default(),
            entered: entered_tx,
            release: Notify::new(),
        });
        let repl = Arc::new(CodeMode::new(
            directory.path().to_owned(),
            Arc::new(ArtifactStore::new(
                directory.path(),
                directory.path().join("artifacts"),
            )?),
        )?);
        let mut base = ToolRegistry::new();
        let processes = register_native_tools(&mut base, directory.path(), false)?;
        let effects = Arc::new(AtomicUsize::new(0));
        for (name, read_only) in [("effect", false), ("old_read", true)] {
            base.register_host(Arc::new(Capability {
                name,
                read_only,
                calls: effects.clone(),
            }))?;
        }
        let tools = Arc::new(RwLock::new(base.clone()));
        let source = Arc::downgrade(&tools);
        let policy = Arc::new(RwLock::new(allow()));
        let approval = policy.clone();
        let config = HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            registry_resolver: Some(Arc::new(move || {
                source
                    .upgrade()
                    .map(|source| source.read().unwrap().clone())
                    .unwrap_or_default()
            })),
            approval_resolver: Some(Arc::new(move |_| approval.read().unwrap().clone())),
            approval: allow(),
            ..Default::default()
        };
        let manager = child_agent_host(
            store.clone(),
            probe.clone(),
            &base,
            &repl,
            &config,
            directory.path(),
            true,
            None,
            Agents {
                max_depth: depth,
                auto_wake: false,
                ..Default::default()
            },
            Arc::new(AgentRegistry::default()),
            processes,
        )?;
        manager.register(&mut base)?;
        register_agent_handles(&mut base, manager.clone())?;
        repl.register(&mut base)?;
        *tools.write().unwrap() = base.clone();
        let parent = Harness::new(store.clone(), probe.clone(), base, config);
        let owner = ToolContext {
            session_id: root.id,
            run_id: "fixture".into(),
            invocation_id: "fixture".into(),
            cancellation: CancellationToken::new(),
            approval: allow(),
        };
        Ok(Self {
            _directory: directory,
            store,
            parent,
            manager,
            tools,
            policy,
            probe,
            entered,
            effects,
            owner,
            _repl: repl,
        })
    }
    async fn settle(&self, owner: &str, receipt: &Value) -> Result<Value> {
        let result = self
            .manager
            .wait(owner, receipt["sessionId"].as_str().unwrap(), Some(5000))
            .await?;
        assert_ne!(result["status"], "timeout", "{result}");
        Ok(result)
    }
    fn description(&self, session: &str) -> String {
        self.probe
            .requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|request| request.session_id == session)
            .unwrap()
            .tools[0]
            .description
            .clone()
    }
}

#[tokio::test]
async fn depth_limit_can_grow_from_zero_and_shrink_without_removing_profiles() -> Result<()> {
    let fixture = Fixture::new(Some(0))?;
    assert!(
        fixture
            .manager
            .start(&fixture.owner, json!({"task":"first"}))
            .unwrap_err()
            .to_string()
            .contains("depth limit")
    );
    fixture.manager.apply_limits(&Agents {
        max_depth: Some(1),
        auto_wake: false,
        ..Default::default()
    });
    let child = fixture
        .manager
        .start(&fixture.owner, json!({"task":"first"}))?;
    fixture.settle(&fixture.owner.session_id, &child).await?;
    let child_id = child["sessionId"].as_str().unwrap();
    let child_owner = ToolContext {
        session_id: child_id.into(),
        ..fixture.owner.clone()
    };
    assert!(
        fixture
            .manager
            .start(&child_owner, json!({"task":"grandchild"}))
            .unwrap_err()
            .to_string()
            .contains("depth limit")
    );
    fixture.manager.apply_limits(&Agents {
        max_depth: Some(3),
        auto_wake: false,
        ..Default::default()
    });
    let grandchild = fixture
        .manager
        .start(&child_owner, json!({"task":"grandchild"}))?;
    fixture.settle(child_id, &grandchild).await?;
    assert!(
        fixture
            .description(grandchild["sessionId"].as_str().unwrap())
            .contains("agent_send")
    );
    fixture.manager.apply_limits(&Agents {
        max_depth: Some(0),
        auto_wake: false,
        ..Default::default()
    });
    assert!(
        fixture
            .manager
            .start(&fixture.owner, json!({"task":"blocked"}))
            .is_err()
    );
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child_id,"text":"continue existing child"}),
    )?;
    fixture.settle(&fixture.owner.session_id, &child).await?;
    fixture.manager.shutdown().await?;
    Ok(())
}

#[path = "live_agent_settings/tool_inventory.rs"]
mod tool_inventory;

#[tokio::test]
async fn running_child_inherits_live_parent_approval_but_keeps_explicit_parent_override()
-> Result<()> {
    for pinned in [false, true] {
        let mut fixture = Fixture::new(None)?;
        fixture
            .parent
            .prompt(
                &fixture.owner.session_id,
                "spawn",
                RunOptions {
                    script: Some(
                        "return await tools.agent({task:'gated effect',profile:'workspace'});"
                            .into(),
                    ),
                    approval: pinned.then(allow),
                    ..Default::default()
                },
            )
            .await?;
        let child = tokio::time::timeout(Duration::from_secs(5), fixture.entered.recv())
            .await?
            .unwrap();
        fixture
            .policy
            .write()
            .unwrap()
            .tools
            .insert("effect".into(), ApprovalMode::Deny);
        fixture.probe.release.notify_one();
        fixture
            .manager
            .wait(&fixture.owner.session_id, &child, Some(5000))
            .await?;
        assert_eq!(fixture.effects.load(Ordering::SeqCst), usize::from(pinned));
        let messages = fixture.store.messages(&child)?;
        assert!(messages.iter().any(|message| message.role == Role::Tool));
        fixture.manager.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn live_research_profile_can_enable_after_startup_without_exposing_parent_tools() -> Result<()>
{
    use bubblepaw_runner::{
        config::{EffectiveConfig, LoadOptions},
        profiles::child_agent_host_live,
        provider::LiveProviderConfig,
    };
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("config.toml");
    let load = |enabled: bool| -> Result<EffectiveConfig> {
        let read_only = !enabled;
        std::fs::write(
            &path,
            format!(
                "[model]\nmain='local/fixture'\n[providers.local]\nkind='scripted'\n[web]\nenabled={enabled}\n[web.expose]\nfetch=false\nsearch=false\n[approvals]\nread_only={read_only}\n"
            ),
        )?;
        EffectiveConfig::load(LoadOptions {
            workspace: directory.path().to_owned(),
            home: None,
            custom_file: Some(path.clone()),
            env: vec![],
            cli: vec![],
        })
    };
    let live = Arc::new(LiveProviderConfig::new(load(false)?)?);
    let store = Store::open_data_dir(directory.path().join("data"))?;
    let parent = store.create_session("parent")?;
    let (entered, _) = mpsc::unbounded_channel();
    let probe = Arc::new(Probe {
        requests: Mutex::default(),
        entered,
        release: Notify::new(),
    });
    let repl = Arc::new(CodeMode::new(
        directory.path().to_owned(),
        Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?),
    )?);
    let mut base = ToolRegistry::new();
    let processes = register_native_tools(&mut base, directory.path(), false)?;
    base.register_host(Arc::new(Capability {
        name: "mcp__private__read",
        read_only: true,
        calls: Arc::new(AtomicUsize::new(0)),
    }))?;
    let source = base.clone();
    let config = HarnessConfig {
        execution_mode: bubblepaw_core::ExecutionMode::Js,
        registry_resolver: Some(Arc::new(move || source.clone())),
        ..Default::default()
    };
    let manager = child_agent_host_live(
        store,
        probe.clone(),
        &base,
        &repl,
        &config,
        directory.path(),
        true,
        None,
        Agents {
            auto_wake: false,
            ..Default::default()
        },
        Arc::new(AgentRegistry::default()),
        processes,
        live.clone(),
    )?;
    let owner = ToolContext {
        session_id: parent.id,
        run_id: "fixture".into(),
        invocation_id: "fixture".into(),
        cancellation: CancellationToken::new(),
        approval: allow(),
    };
    assert!(manager.profile("research").is_none());
    assert!(manager.profile("workspace").is_none());
    assert!(
        manager
            .start(&owner, json!({"task":"inspect","profile":"workspace"}))
            .is_err()
    );
    let mut initial = ToolRegistry::new();
    manager.register(&mut initial)?;
    let initial_schema = initial.get("agent").unwrap().definition().input_schema;
    assert!(
        !initial_schema["properties"]["profile"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("workspace"))
    );
    assert!(
        manager
            .start(&owner, json!({"task":"inspect","profile":"research"}))
            .is_err()
    );
    live.apply_prepared(live.prepare(load(true)?)?);
    assert!(manager.profile("research").is_some());
    assert!(manager.profile("workspace").is_some());
    let mut registered = ToolRegistry::new();
    manager.register(&mut registered)?;
    assert!(
        registered.get("agent").unwrap().definition().input_schema["properties"]["profile"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("research"))
    );
    let enabled_schema = registered.get("agent").unwrap().definition().input_schema;
    assert!(
        enabled_schema["properties"]["profile"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("workspace"))
    );
    assert_eq!(
        initial.get("agent").unwrap().definition().input_schema,
        initial_schema
    );
    let child = manager.start(&owner, json!({"task":"inspect","profile":"research"}))?;
    let child_id = child["sessionId"].as_str().unwrap();
    assert_ne!(
        manager
            .wait(&owner.session_id, child_id, Some(5000))
            .await?["status"],
        "timeout"
    );
    let description = probe.requests.lock().unwrap().last().unwrap().tools[0]
        .description
        .clone();
    assert!(description.contains("web_search"));
    assert!(description.contains("web_fetch"));
    assert!(!description.contains("mcp__private__read"));
    assert!(description.contains("agent_send"));
    assert!(description.contains("agent_status"));
    let workspace = manager.start(
        &owner,
        json!({"task":"inspect workspace","profile":"workspace"}),
    )?;
    let workspace_id = workspace["sessionId"].as_str().unwrap();
    assert_ne!(
        manager
            .wait(&owner.session_id, workspace_id, Some(5000))
            .await?["status"],
        "timeout"
    );
    live.apply_prepared(live.prepare(load(false)?)?);
    assert!(manager.profile("research").is_none());
    assert!(manager.profile("workspace").is_none());
    assert!(
        manager
            .start(
                &owner,
                json!({"task":"blocked workspace","profile":"workspace"})
            )
            .is_err()
    );
    let mut disabled = ToolRegistry::new();
    manager.register(&mut disabled)?;
    let disabled_schema = disabled.get("agent").unwrap().definition().input_schema;
    assert!(
        !disabled_schema["properties"]["profile"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("workspace"))
    );
    assert_eq!(
        registered.get("agent").unwrap().definition().input_schema,
        enabled_schema
    );
    assert_eq!(
        manager.status(&owner.session_id, &json!({"sessionId":workspace_id}))?["status"],
        "idle"
    );
    manager.send(
        &owner,
        json!({"sessionId":workspace_id,"text":"continue existing workspace child"}),
    )?;
    assert_ne!(
        manager
            .wait(&owner.session_id, workspace_id, Some(5000))
            .await?["status"],
        "timeout"
    );
    assert_eq!(
        manager.status(&owner.session_id, &json!({"sessionId":child_id}))?["status"],
        "idle"
    );
    manager.send(
        &owner,
        json!({"sessionId":child_id,"text":"continue existing research"}),
    )?;
    assert_ne!(
        manager
            .wait(&owner.session_id, child_id, Some(5000))
            .await?["status"],
        "timeout"
    );
    manager.shutdown().await?;
    repl.shutdown().await;
    Ok(())
}

#[path = "live_agent_settings/deny_reload.rs"]
mod deny_reload;
