use super::*;

#[tokio::test]
async fn worker_history_rounds_keep_long_answer_and_leave_source_view_unchanged() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.sqlite");
    let store = Store::open(&path)?;
    let session = store
        .create_session_with_strategy("fixture", bubblepaw_core::ContextStrategy::Classical)?;
    let history = vec![message("r1", &session.id)];
    store.append_batch(&history)?;
    let frozen = ModelRequest {
        system_prompt: "fixture".into(),
        messages: history,
        ..Default::default()
    };
    store.save_context_checkpoint(&bubblepaw_core::ContextCheckpoint {
        id: "fixture-checkpoint".into(),
        session_id: session.id.clone(),
        through_message_id: "r1".into(),
        model: "scripted".into(),
        summary: "Fixture fact was blue.".into(),
        files_touched: vec![],
        created_at: "2026-10-09T00:00:00Z".into(),
        usage: None,
    })?;
    store.save_context_view(&bubblepaw_core::ContextView {
        session_id: session.id.clone(),
        frontier: Some(store.message_seq(&session.id, "r1")?),
        system_prompt: frozen.system_prompt.clone(),
        prefix: frozen.messages.clone(),
        masked_tool_output_ids: vec![],
        clipped_tool_call_ids: vec![],
        created_at: "2026-10-09T00:00:00Z".into(),
        threshold: 1000,
        budget: 500,
        tokens: 100,
        blocks: Vec::new(),
        blocks_estimated: true,
    })?;
    let before = serde_json::to_value(store.snapshot(&session.id, 100, 100, 100)?)?;
    let before_view = serde_json::to_value(store.context_view(&session.id)?)?;
    let before_tx = store.storage_stats()?.transaction_count;
    let context = crate::summarization::SummarizingContext::new(
        store.clone(),
        std::sync::Arc::new(crate::provider::ScriptedProvider::new("must not call")),
        crate::summarization::SummaryConfig::default(),
        None,
    )?;
    let frozen = context.built_frozen_request(&session.id, frozen).await?;
    assert_eq!(store.storage_stats()?.transaction_count, before_tx);
    let long = "blue ".repeat(20_000);
    let model = Arc::new(FakeModel {
        replies: Mutex::new(VecDeque::from([
            tool_response("history_read", 1),
            tool_response("history_read", 2),
            tool_response("history_read", 3),
            response(json!({"role":"assistant","content":long})),
        ])),
        requests: Mutex::new(vec![]),
        closed: Mutex::new(Vec::new()),
    });
    let answer = answer(model.clone(), &store, &session.id, &frozen, &probe()).await;
    let worker_debug = answer.worker_session_id().map(|id| {
        store.runs(id).unwrap().last().map(|run| {
            // This fixture uses only FakeModel; expose synthetic errors without
            // copying its large answer or any request payload into diagnostics.
            json!({"status":run.status,"error":run.error,"answer_bytes":run.text.as_ref().map(String::len)})
        })
    });
    assert_eq!(
        answer["status"], "answered",
        "Synthetic worker: {worker_debug:?}"
    );
    assert_eq!(answer["answer"], long);
    assert_eq!(answer["tool_calls"].as_array().unwrap().len(), 3);
    assert!(
        answer["tool_calls"][0]["result"]
            .as_str()
            .unwrap()
            .contains("blue")
    );
    let (_, _, worker_id, run_id) = answer.worker.as_ref().unwrap();
    let worker_id = worker_id.clone();
    assert_eq!(answer.worker_session_id(), Some(worker_id.as_str()));
    assert_eq!(answer["worker_session_id"], worker_id);
    assert_eq!(answer.worker_run_id(), Some(run_id.as_str()));
    assert_eq!(answer["worker_run_id"], run_id.as_str());
    assert_eq!(store.runs(&worker_id)?.last().unwrap().id, *run_id);
    assert!(store.session(&worker_id)?.hidden);
    assert_eq!(
        model.closed.lock().unwrap().as_slice(),
        std::slice::from_ref(&worker_id)
    );
    assert_eq!(store.list_visible_sessions()?.len(), 1);
    let worker_history = store.messages(&worker_id)?;
    assert_eq!(worker_history.len(), 8);
    assert_eq!(
        worker_history[1].provider_state,
        Some(json!({"opaque_signature":"probe-signature-byte-exact"}))
    );
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0]["_bubblepaw_request"]["session_id"], worker_id);
        assert!(
            !requests[0]["_bubblepaw_request"]["history"]["epoch"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        for request in requests.iter().take(3) {
            let names: Vec<_> = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                vec!["call_tool", "history_read", "history_search", "tool_search"]
            );
            assert_eq!(request["reasoning_effort"], "low");
        }
        // Exhausting the work budget keeps the declared prefix byte-stable.
        assert_eq!(requests[3]["tools"], requests[0]["tools"]);
        assert_eq!(
            requests[1]["messages"][3]["provider_state"],
            json!({"opaque_signature":"probe-signature-byte-exact"})
        );
    }
    let mut after = store.snapshot(&session.id, 100, 100, 100)?;
    let before_revision = before["revision"].as_u64().unwrap();
    // Canonical worker writes advance the shared store's global revision.
    // Every source-session field must otherwise remain exactly unchanged.
    assert!(after.revision > before_revision);
    after.revision = before_revision;
    assert_eq!(serde_json::to_value(after)?, before);
    assert_eq!(
        serde_json::to_value(store.context_view(&session.id)?)?,
        before_view
    );
    assert!(store.storage_stats()?.transaction_count > before_tx);
    answer.acknowledge().await?;
    assert!(store.session(&worker_id).is_err());
    assert_eq!(store.messages(&session.id)?.len(), 1);
    assert_eq!(frozen.messages.len(), 1);
    Ok(())
}
