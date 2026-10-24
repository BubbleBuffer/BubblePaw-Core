use super::*;

#[test]
fn cached_request_size_distinguishes_signed_zero_in_same_id_json_fields() -> Result<()> {
    let mut cache = ParsedHistory::default();
    let mut message = Store::message("s", Role::Assistant, "reply".into());
    message.tool_calls = vec![crate::ToolCall {
        id: "c".into(),
        name: "echo".into(),
        arguments: serde_json::json!({"nested":[{"value":0.0}]}),
    }];
    message.provider_state = Some(serde_json::json!({"nested":[{"value":0.0}]}));
    let mut request = ModelRequest {
        messages: vec![message],
        ..Default::default()
    };
    assert_eq!(
        cache.request_bytes(&mut request)?,
        serde_json::to_vec(&request)?.len()
    );
    for value in [-0.0, 0.0, -0.0, 0.0] {
        request.messages[0].tool_calls[0].arguments["nested"][0]["value"] =
            serde_json::json!(value);
        assert_eq!(
            cache.request_bytes(&mut request)?,
            serde_json::to_vec(&request)?.len()
        );
    }
    for value in [-0.0, 0.0, -0.0, 0.0] {
        request.messages[0].provider_state.as_mut().unwrap()["nested"][0]["value"] =
            serde_json::json!(value);
        assert_eq!(
            cache.request_bytes(&mut request)?,
            serde_json::to_vec(&request)?.len()
        );
    }
    Ok(())
}

#[test]
fn cached_request_size_is_exact_and_tracks_same_id_transformations() -> Result<()> {
    let mut cache = ParsedHistory::default();
    let mut first = Store::message("s", Role::User, "quotes: \" \\ \n snowman: ☃".into());
    let mut second = Store::message("s", Role::Assistant, "reply".into());
    second.tool_calls = vec![crate::ToolCall {
        id: "c".into(),
        name: "echo".into(),
        arguments: serde_json::json!({"text":"first"}),
    }];
    let mut request = ModelRequest {
        model: "test".into(),
        system_prompt: "quoted \" shell".into(),
        messages: vec![first.clone(), second.clone()],
        ..Default::default()
    };
    for index in 0..5 {
        match index {
            1 => {
                request.max_output_tokens = Some(100);
            }
            2 => {
                first.content = "same id, longer transformed content".repeat(200);
                request.messages[0] = first.clone();
            }
            3 => {
                second.tool_calls[0].arguments =
                    serde_json::json!({"new":"longer arguments","escape":"\n\""});
                request.messages[1] = second.clone();
            }
            4 => {
                request.messages[1].provider_state =
                    Some(serde_json::json!({"signature":"opaque signature"}));
            }
            _ => {}
        }
        assert_eq!(
            cache.request_bytes(&mut request)?,
            serde_json::to_vec(&request)?.len()
        );
        assert_eq!(
            cache.request_bytes(&mut request)?,
            serde_json::to_vec(&request)?.len()
        );
    }
    request.messages.clear();
    assert_eq!(
        cache.request_bytes(&mut request)?,
        serde_json::to_vec(&request)?.len()
    );
    request.messages.push(first);
    assert_eq!(
        cache.request_bytes(&mut request)?,
        serde_json::to_vec(&request)?.len()
    );
    Ok(())
}

#[test]
fn parsed_history_appends_the_indexed_tail_and_keeps_existing_messages() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("history")?;
    let first = Store::message(&session.id, Role::User, "first".into());
    store.append(&first)?;
    let mut history = ParsedHistory::default();
    history.refresh(&store, &session.id)?;
    let through = history.through;
    assert_eq!(history.messages, vec![first.clone()]);
    let unrelated = store.create_session("other")?;
    store.append(&Store::message(&unrelated.id, Role::User, "other".into()))?;
    let second = Store::message(&session.id, Role::Assistant, "second".into());
    store.append(&second)?;
    history.refresh(&store, &session.id)?;
    assert!(history.through > through);
    assert_eq!(history.messages, vec![first, second]);
    history.refresh(&store, &session.id)?;
    assert_eq!(history.messages, store.messages(&session.id)?);
    Ok(())
}

#[tokio::test]
async fn prepared_guard_preserves_large_chat_journal_and_rejects_active_payload() -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Recording(AtomicUsize);
    #[async_trait::async_trait]
    impl Provider for Recording {
        async fn generate(
            &self,
            request: ModelRequest,
            _: TextCallback,
            _: CancellationToken,
        ) -> Result<crate::ModelResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            assert!(serde_json::to_vec(&request)?.len() > 2 * 1024 * 1024);
            assert!(crate::prepared_request_bytes(&request)? < 2 * 1024 * 1024);
            Ok(crate::ModelResponse {
                text: "done".into(),
                ..Default::default()
            })
        }
    }
    let store = Store::in_memory()?;
    let session = store.create_session("journal")?.id;
    let mut archived = Store::message(&session, Role::Assistant, "saved".into());
    archived.provider_state = Some(serde_json::json!({
        "adapter":"chat-completions", "message_fields":{"reasoning_content":"native"},
        "opaque_stream_fragments":[{"event":"x".repeat(3 * 1024 * 1024)}]
    }));
    store.append(&archived)?;
    let provider = Arc::new(Recording(AtomicUsize::new(0)));
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        crate::ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let mut cache = ParsedHistory::default();
    let mut request = ModelRequest {
        messages: vec![archived.clone()],
        ..Default::default()
    };
    assert_eq!(
        cache.request_bytes(&mut request)?,
        crate::prepared_request_bytes(&request)?
    );
    assert_eq!(request.messages[0], archived);
    assert_eq!(
        harness
            .prompt(&session, "small", crate::RunOptions::default())
            .await?
            .status,
        "completed"
    );
    assert_eq!(store.messages(&session)?[0], archived);
    let oversized_session = store.create_session("oversized initial prompt")?.id;
    let mut preserved = archived.clone();
    preserved.session_id = oversized_session.clone();
    preserved.id = id();
    store.append(&preserved)?;
    let failed = harness
        .prompt(
            &oversized_session,
            "small",
            crate::RunOptions {
                system_prompt: Some("active".repeat(512 * 1024)),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(failed.status, "failed");
    assert!(
        failed
            .error
            .unwrap()
            .contains("Prepared request exceeds configured 2097152 byte limit")
    );
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    Ok(())
}
