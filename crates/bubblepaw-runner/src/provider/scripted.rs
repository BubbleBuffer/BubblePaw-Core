use anyhow::{Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ModelInfo, ModelRequest, ModelResponse, Provider, TextCallback, ToolCall,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    sync::Mutex,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScriptedStep {
    Text { text: String },
    Calls { calls: Vec<ToolCall> },
}
/// Deterministic fixture provider; executes the exact native tool path used by
/// real providers. Once the queued calls finish, it returns the final text.
pub struct ScriptedProvider {
    steps: Mutex<VecDeque<ScriptedStep>>,
    final_text: String,
    require_success: bool,
    emitted_call_ids: Mutex<HashSet<String>>,
    demo_writable: Mutex<Option<bool>>,
}
impl ScriptedProvider {
    /// Offline demo using the tool surface selected for the session.
    /// Writable demo creates one new file; it never overwrites.
    pub fn demo(writable: bool) -> Self {
        let mut provider = Self::new("Offline demo completed through native capabilities.");
        provider.require_success = true;
        *provider.demo_writable.get_mut().unwrap() = Some(writable);
        provider
    }
    fn demo_steps(writable: bool, js: bool) -> Vec<ScriptedStep> {
        let mut code = String::from(
            "const listing = await tools.list({path:'.',limit:20}); text(listing.entries.map(entry=>entry.name).join('\\n'));\n",
        );
        if writable {
            let path = format!("bubblepaw-demo-{}.txt", uuid::Uuid::new_v4());
            #[cfg(windows)]
            let command = "[Console]::Out.Write('BubblePaw process tool executed')";
            #[cfg(not(windows))]
            let command = "printf 'BubblePaw process tool executed'";
            code.push_str(&format!(
                "const path={}; const content='BubblePaw base executed a native tool.\\n';\n\
                 await tools.write({{path,content}}); const read=await tools.read({{path}});\n\
                 if(read.content!==content) throw new Error('Demo file content mismatch'); text(read.content);\n\
                 const job=await tools.exec({{command:{},yieldMs:1000,timeoutMs:30000}});\n\
                 const result=job.status==='running'?await wait({{handle:job.handle}}):job;\n\
                 if(result.status!=='exited'||result.exitCode!==0) throw new Error(result.diagnostic||'Demo command failed');\n",
                serde_json::to_string(&path).unwrap(),
                serde_json::to_string(command).unwrap()
            ));
        }
        if js {
            return vec![ScriptedStep::Calls {
                calls: vec![ToolCall {
                    id: format!("demo-js-{}", uuid::Uuid::new_v4()),
                    name: "js".into(),
                    arguments: serde_json::json!({"code":code}),
                }],
            }];
        }
        let call = |name: &str, arguments| ToolCall {
            id: format!("demo-{name}-{}", uuid::Uuid::new_v4()),
            name: name.into(),
            arguments,
        };
        let mut calls = vec![call("list", serde_json::json!({"path":".","limit":20}))];
        if writable {
            let path = format!("bubblepaw-demo-{}.txt", uuid::Uuid::new_v4());
            #[cfg(windows)]
            let command = "[Console]::Out.Write('BubblePaw process tool executed')";
            #[cfg(not(windows))]
            let command = "printf 'BubblePaw process tool executed'";
            calls.extend([
                call("write", serde_json::json!({"path":path,"content":"BubblePaw base executed a native tool.\n"})),
                call("read", serde_json::json!({"path":path})),
                call("exec", serde_json::json!({"command":command,"yieldMs":30000,"timeoutMs":30000})),
            ]);
        }
        // This fixed sequence needs no result-dependent planning. Native dispatch
        // preserves the write/read order inside one assistant call batch.
        vec![ScriptedStep::Calls { calls }]
    }
    pub fn new(final_text: impl Into<String>) -> Self {
        Self {
            steps: Mutex::new(VecDeque::new()),
            final_text: final_text.into(),
            require_success: false,
            emitted_call_ids: Mutex::new(HashSet::new()),
            demo_writable: Mutex::new(None),
        }
    }
    pub fn with_steps(steps: Vec<ScriptedStep>, final_text: impl Into<String>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            final_text: final_text.into(),
            require_success: false,
            emitted_call_ids: Mutex::new(HashSet::new()),
            demo_writable: Mutex::new(None),
        }
    }
}
#[async_trait]
impl Provider for ScriptedProvider {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "scripted".into(),
            name: "Scripted (offline fixture)".into(),
        }]
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        ensure!(!cancellation.is_cancelled(), "cancelled");
        if self.require_success {
            if let Some(writable) = self
                .demo_writable
                .lock()
                .map_err(|_| anyhow::anyhow!("scripted provider lock poisoned"))?
                .take()
            {
                let js = request.tools.iter().any(|tool| tool.name == "js");
                if js {
                    ensure!(request.tools.len() == 1, "offline JS demo expects only js");
                } else {
                    for name in if writable {
                        &["list", "write", "read", "exec"][..]
                    } else {
                        &["list"][..]
                    } {
                        ensure!(
                            request.tools.iter().any(|tool| tool.name == *name),
                            "offline demo missing {name}"
                        );
                    }
                }
                *self
                    .steps
                    .lock()
                    .map_err(|_| anyhow::anyhow!("scripted provider lock poisoned"))? =
                    Self::demo_steps(writable, js).into();
            }
            // A resumed transcript can contain unknown outcomes from an older
            // provider. Validate only calls this fixture actually emitted.
            let emitted = self
                .emitted_call_ids
                .lock()
                .map_err(|_| anyhow::anyhow!("scripted provider lock poisoned"))?;
            for message in request.messages.iter().filter(|message| {
                message.role == bubblepaw_core::Role::Tool
                    && message
                        .tool_call_id
                        .as_ref()
                        .is_some_and(|id| emitted.contains(id))
            }) {
                ensure!(
                    !message.is_error,
                    "offline demo tool failed: {}",
                    message.content
                );
                if message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("demo-exec-"))
                {
                    let result: serde_json::Value =
                        serde_json::from_str(message.content.split("\n[---]\n").next().unwrap())?;
                    ensure!(
                        result["status"] == "exited" && result["exitCode"] == 0,
                        "offline demo command did not exit successfully: {}",
                        message.content
                    );
                }
            }
        }
        let step = self
            .steps
            .lock()
            .map_err(|_| anyhow::anyhow!("scripted provider lock poisoned"))?
            .pop_front();
        match step {
            Some(ScriptedStep::Calls { calls }) => {
                if self.require_success {
                    self.emitted_call_ids
                        .lock()
                        .map_err(|_| anyhow::anyhow!("scripted provider lock poisoned"))?
                        .extend(calls.iter().map(|call| call.id.clone()));
                }
                Ok(ModelResponse {
                    quota: Vec::new(),
                    text: String::new(),
                    tool_calls: calls,
                    usage: None,
                    last_request_usage: None,
                    provider_state: None,
                })
            }
            Some(ScriptedStep::Text { text }) => {
                on_text(&text);
                Ok(ModelResponse {
                    quota: Vec::new(),
                    text,
                    tool_calls: Vec::new(),
                    usage: None,
                    last_request_usage: None,
                    provider_state: None,
                })
            }
            None => {
                on_text(&self.final_text);
                Ok(ModelResponse {
                    quota: Vec::new(),
                    text: self.final_text.clone(),
                    tool_calls: Vec::new(),
                    usage: None,
                    last_request_usage: None,
                    provider_state: None,
                })
            }
        }
    }
}
