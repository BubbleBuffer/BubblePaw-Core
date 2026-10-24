//! Scripted providers only; no network/model calls.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role, Store,
    TextCallback, ToolCall, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    agents::AgentTools, artifacts::ArtifactStore, codemode::CodeMode, net::WebConfig,
    profiles::workspace_profiles, research::research_profile_with_tools,
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct LongConversation {
    requests: Mutex<HashMap<String, usize>>,
}
#[async_trait]
impl Provider for LongConversation {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        *self
            .requests
            .lock()
            .unwrap()
            .entry(request.session_id.clone())
            .or_default() += 1;
        let round = request
            .messages
            .iter()
            .rev()
            .take_while(|message| message.role != Role::User)
            .filter(|message| message.role == Role::Tool)
            .count();
        Ok(if round < 14 {
            ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":"return 'next round';"}),
                }],
                ..Default::default()
            }
        } else {
            ModelResponse {
                text: "finished naturally".into(),
                ..Default::default()
            }
        })
    }
}

#[tokio::test]
async fn child_paths_inherit_optional_limits_without_a_twelve_round_ceiling() -> Result<()> {
    for limit in [None, Some(2)] {
        let root = tempfile::tempdir()?;
        let store = Store::in_memory()?;
        let provider = Arc::new(LongConversation::default());
        let config = HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            max_rounds: limit,
            ..Default::default()
        };
        let repl = Arc::new(CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(
                root.path(),
                root.path().join("artifacts"),
            )?),
        )?);
        let tools = ToolRegistry::new();
        let mut profiles = workspace_profiles(
            store.clone(),
            provider.clone(),
            &tools,
            &repl,
            &config,
            root.path(),
            true,
        )?;
        profiles.push(research_profile_with_tools(
            store.clone(),
            provider.clone(),
            &tools,
            &repl,
            &config,
            &WebConfig::default(),
        )?);
        assert!(profiles.iter().all(|profile| profile.max_rounds == limit));
        assert!(
            profiles
                .iter()
                .all(|profile| profile.harness.max_rounds() == limit)
        );
        let manager = AgentTools::from_profiles(store.clone(), profiles.clone())?;
        let context = ToolContext {
            session_id: store.create_session("parent")?.id,
            run_id: "parent-run".into(),
            invocation_id: "child-call".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        };
        let expected_status = if limit.is_none() {
            "completed"
        } else {
            "failed"
        };
        let expected_requests = limit.unwrap_or(15);
        for profile in &profiles {
            let child = manager.start(
                &context,
                json!({"profile":profile.name,"task":"fourteen tool rounds"}),
            )?;
            let address = child["sessionId"].as_str().unwrap();
            assert_eq!(
                manager
                    .wait(&context.session_id, address, Some(5000))
                    .await?["status"],
                expected_status
            );
            assert_eq!(
                provider.requests.lock().unwrap()[address],
                expected_requests
            );
            manager.send(
                &context,
                json!({"sessionId":address,"text":"another fourteen rounds"}),
            )?;
            assert_eq!(
                manager
                    .wait(&context.session_id, address, Some(5000))
                    .await?["status"],
                expected_status
            );
            assert_eq!(
                provider.requests.lock().unwrap()[address],
                2 * expected_requests
            );
        }
        // A profile's own round ceiling applies to a worker started from that profile.
        let worker = manager.start_worker(
            &context,
            profiles[0].clone(),
            json!({"task":"fourteen rounds"}),
        )?;
        let address = worker["sessionId"].as_str().unwrap().to_owned();
        assert_eq!(
            manager.wait(&context.session_id, &address, None).await?["status"],
            expected_status
        );
        assert_eq!(
            provider.requests.lock().unwrap()[&address],
            expected_requests
        );

        if limit.is_none() {
            let mut bounded_profile = profiles[0].clone();
            bounded_profile.max_rounds = Some(2);
            let bounded = manager.start_worker(
                &context,
                bounded_profile,
                json!({"task":"fourteen rounds"}),
            )?;
            let address = bounded["sessionId"].as_str().unwrap().to_owned();
            assert_eq!(
                manager.wait(&context.session_id, &address, None).await?["status"],
                "failed"
            );
            assert_eq!(provider.requests.lock().unwrap()[&address], 2);
        }

        // The compatibility native-child entry point derives its profile from the harness.
        let mut native_tools = ToolRegistry::new();
        repl.register(&mut native_tools)?;
        let native = AgentTools::new(
            store.clone(),
            Harness::new(store.clone(), provider.clone(), native_tools, config),
            None,
        );
        let child = native.start(&context, json!({"task":"native child fourteen rounds"}))?;
        let address = child["sessionId"].as_str().unwrap();
        assert_eq!(
            native
                .wait(&context.session_id, address, Some(5000))
                .await?["status"],
            expected_status
        );
        assert_eq!(
            provider.requests.lock().unwrap()[address],
            expected_requests
        );
        native.shutdown().await?;
        manager.shutdown().await?;
        repl.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn reattached_child_refreshes_saved_round_policy_from_the_current_profile() -> Result<()> {
    for current_limit in [None, Some(2)] {
        let root = tempfile::tempdir()?;
        let store = Store::in_memory()?;
        let provider = Arc::new(LongConversation::default());
        let repl = Arc::new(CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(
                root.path(),
                root.path().join("artifacts"),
            )?),
        )?);
        let profiles = |max_rounds| {
            workspace_profiles(
                store.clone(),
                provider.clone(),
                &ToolRegistry::new(),
                &repl,
                &HarnessConfig {
                    execution_mode: bubblepaw_core::ExecutionMode::Js,
                    max_rounds,
                    ..Default::default()
                },
                root.path(),
                false,
            )
        };
        let context = ToolContext {
            session_id: store.create_session("parent")?.id,
            run_id: "parent-run".into(),
            invocation_id: "child-call".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        };
        let old_host = AgentTools::from_profiles(store.clone(), profiles(Some(12))?)?;
        let child = old_host.start(&context, json!({"task":"fourteen tool rounds"}))?;
        let address = child["sessionId"].as_str().unwrap();
        assert_eq!(
            old_host
                .wait(&context.session_id, address, Some(5000))
                .await?["status"],
            "failed"
        );
        assert_eq!(provider.requests.lock().unwrap()[address], 12);
        let (metadata, _) = store
            .attachment(address, "bubblepaw-agent-binding-v1")?
            .unwrap();
        let saved: serde_json::Value = serde_json::from_str(&metadata)?;
        assert_eq!(saved["max_rounds"], 12);
        old_host.shutdown().await?;
        drop(old_host);

        let new_host = AgentTools::from_profiles(store.clone(), profiles(current_limit)?)?;
        // Reattachment restores the child identity and original capabilities;
        // its former host-policy snapshot must not become a permanent cap.
        new_host.status(&context.session_id, &json!({"sessionId":address}))?;
        new_host.send(
            &context,
            json!({"sessionId":address,"text":"another fourteen rounds"}),
        )?;
        let reply = new_host
            .wait(&context.session_id, address, Some(5000))
            .await?;
        assert_eq!(
            reply["status"],
            if current_limit.is_none() {
                "completed"
            } else {
                "failed"
            }
        );
        assert_eq!(
            provider.requests.lock().unwrap()[address],
            12 + current_limit.unwrap_or(15)
        );
        new_host.shutdown().await?;
        repl.shutdown().await;
    }
    Ok(())
}
