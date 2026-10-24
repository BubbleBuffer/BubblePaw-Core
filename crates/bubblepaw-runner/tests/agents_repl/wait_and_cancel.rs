use super::*;

#[tokio::test]
async fn idle_send_recreates_child_js_heap_and_releases_each_turn() -> Result<()> {
    let fixture = ControlFixture::new()?;
    let task = "CELL:if(state.previous!==undefined) throw Error('inherited heap'); state.previous='first'; text('first heap'); return true;";
    let first = format!(
        "const child=await tools.agent({}); return await wait({{sessionId:child.sessionId}});",
        json!({"task":task})
    );
    fixture
        .harness
        .prompt(
            &fixture.session,
            "first child turn",
            RunOptions {
                script: Some(first),
                ..Default::default()
            },
        )
        .await?;
    let messages = fixture.store.messages(&fixture.session)?;
    let reply = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .unwrap();
    assert!(!reply.is_error, "{}", reply.content);
    let first: Value = serde_json::from_str(&reply.content)?;
    assert_eq!(first["status"], "completed");
    let child_id = first["sessionId"].as_str().unwrap();
    assert!(
        fixture.repl.status(child_id)["environments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let second_task = "CELL:if(state.previous!==undefined) throw Error('prior turn heap survived'); text('second fresh heap'); return true;";
    let second = format!(
        "await tools.agent_send({}); return await wait({{sessionId:{}}});",
        json!({"sessionId":child_id,"text":second_task}),
        serde_json::to_string(child_id)?
    );
    fixture
        .harness
        .prompt(
            &fixture.session,
            "second child turn",
            RunOptions {
                script: Some(second),
                ..Default::default()
            },
        )
        .await?;
    let messages = fixture.store.messages(&fixture.session)?;
    let reply = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .unwrap();
    assert!(!reply.is_error, "{}", reply.content);
    let second: Value = serde_json::from_str(&reply.content)?;
    assert_eq!(second["status"], "completed");
    assert_eq!(second["sessionId"], first["sessionId"]);
    assert_eq!(fixture.store.runs(child_id)?.len(), 2);
    let outputs = fixture.store.outputs(child_id, 128)?;
    assert!(
        outputs
            .iter()
            .any(|output| output.output["text"] == "first heap")
    );
    assert!(
        outputs
            .iter()
            .any(|output| output.output["text"] == "second fresh heap")
    );
    assert!(
        fixture.repl.status(child_id)["environments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(fixture.store.pending_effects(&fixture.session)?.is_empty());
    fixture.close().await
}

#[path = "../agents_repl/nested_cancel.rs"]
mod nested_cancel;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_nested_child_js_obeys_depth_and_parent_cancel_cleans_subtree() -> Result<()> {
    nested_cancel::run().await
}
