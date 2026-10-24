//! Structured child results use the ordinary tool dispatcher, including JS.
use super::*;

fn schema() -> Value {
    json!({"name":"answer","schema":{"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"],"additionalProperties":false}})
}
fn submit(value: Value) -> Step {
    Step::Js(
        String::new(),
        format!("return await tools.submit_result({value});"),
    )
}
fn declares_result(request: &ModelRequest) -> bool {
    assert!(request.output_schema.is_none());
    request.tools.iter().any(|tool| {
        tool.name == "submit_result"
            || (tool.name == "js" && tool.description.contains("tools.submit_result("))
    })
}

#[tokio::test]
async fn submitted_result_validates_and_invalid_arguments_are_corrected() -> Result<()> {
    for answers in [
        vec!["{\"answer\":7}"],
        vec!["{\"answer\":\"wrong\"}", "{\"answer\":7}"],
    ] {
        let count = answers.len();
        let fixture = Fixture::new(
            answers
                .into_iter()
                .map(|v| submit(serde_json::from_str(v).unwrap()))
                .collect(),
        )?;
        let child = fixture.manager.start(
            &fixture.owner,
            json!({"task":"structured answer","output_schema":schema()}),
        )?;
        let result = fixture.wait(&child).await?;
        assert_eq!(result["status"], "completed", "{result}");
        assert_eq!(result["json"], json!({"answer":7}));
        assert_eq!(result["text"], "{\"answer\":7}");
        {
            let requests = fixture.probe.requests.lock().unwrap();
            assert_eq!(requests.len(), count);
            assert!(requests.iter().all(declares_result));
            if count == 2 {
                assert!(
                    requests[1]
                        .messages
                        .iter()
                        .any(|m| m.role == Role::Tool && m.is_error)
                );
            }
        }
        fixture.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn missing_result_failure_can_continue_and_schema_retains_replaces_or_clears() -> Result<()> {
    let fixture = Fixture::new(vec![
        Step::Text("not JSON".into()),
        submit(json!({"answer":"wrong"})),
        Step::Text("still missing the required result".into()),
        submit(json!({"answer":8})),
        submit(json!({"answer":8})),
        submit(json!({"answer":9})),
        Step::Text("plain reply".into()),
    ])?;
    let child = fixture.manager.start(
        &fixture.owner,
        json!({"task":"structured answer","output_schema":schema()}),
    )?;
    let failed = fixture.wait(&child).await?;
    assert_eq!(failed["status"], "failed");
    assert!(
        failed["error"]
            .as_str()
            .is_some_and(|e| e.contains("structured result") || e.contains("submit_result"))
    );
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"try again"}),
    )?;
    assert_eq!(fixture.wait(&child).await?["json"], json!({"answer":8}));
    let mut replacement = schema();
    replacement["name"] = json!("replacement");
    replacement["schema"]["properties"]["answer"]["minimum"] = json!(9);
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"replace schema","output_schema":replacement}),
    )?;
    assert_eq!(fixture.wait(&child).await?["json"], json!({"answer":9}));
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"plain answer","output_schema":null}),
    )?;
    let plain = fixture.wait(&child).await?;
    assert_eq!(plain["status"], "completed");
    assert_eq!(plain["text"], "plain reply");
    assert!(plain.get("json").is_none());
    {
        let requests = fixture.probe.requests.lock().unwrap();
        assert_eq!(requests.len(), 7);
        assert!(requests.iter().all(declares_result));
        for request in &requests[1..] {
            assert_eq!(
                serde_json::to_vec(&request.tools)?,
                serde_json::to_vec(&requests[0].tools)?
            );
            assert_eq!(request.system_prompt, requests[0].system_prompt);
        }
        assert!(requests[4].messages.iter().any(|message| {
            message.content.contains("current definition")
                && message.content.contains("\"minimum\":9")
        }));
        assert!(
            requests[6]
                .messages
                .iter()
                .any(|message| { message.content.contains("This run has no output schema") })
        );
        assert!(
            requests[6]
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content == "structured answer")
        );
        assert!(
            requests[6]
                .messages
                .iter()
                .any(|m| m.role == Role::Assistant && m.content == "{\"answer\":8}")
        );
    }
    fixture.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn oversized_json_is_explicit_failure_and_can_continue_with_full_history() -> Result<()> {
    let huge = json!({"answer":"x".repeat(70_000)}).to_string();
    let fixture = Fixture::new(vec![
        submit(serde_json::from_str(&huge)?),
        Step::Text("plain follow-up".into()),
    ])?;
    let string_schema = json!({"name":"large","schema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}});
    let child = fixture.manager.start(
        &fixture.owner,
        json!({"task":"large JSON","output_schema":string_schema}),
    )?;
    let result = fixture.wait(&child).await?;
    assert_eq!(result["status"], "failed");
    assert!(result.get("json").is_none());
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("reply size limit")
    );
    assert!(result.to_string().len() <= 60 * 1024);
    assert!(
        fixture
            .store
            .messages(child["sessionId"].as_str().unwrap())?
            .iter()
            .any(|m| m.role == Role::Assistant && m.content == huge)
    );
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"continue","output_schema":null}),
    )?;
    assert_eq!(fixture.wait(&child).await?["text"], "plain follow-up");
    fixture.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn reattached_child_restores_schema_and_returns_parsed_json() -> Result<()> {
    let fixture = Fixture::new(vec![
        submit(json!({"answer":1})),
        submit(json!({"answer":2})),
    ])?;
    let child = fixture.manager.start(
        &fixture.owner,
        json!({"task":"JSON","output_schema":schema(),"max_tool_calls":0}),
    )?;
    assert_eq!(fixture.wait(&child).await?["json"], json!({"answer":1}));
    let manager = AgentTools::from_profiles(
        fixture.store.clone(),
        vec![fixture.manager.profile("read-only").unwrap().clone()],
    )?;
    assert_eq!(
        manager.status(
            &fixture.owner.session_id,
            &json!({"sessionId":child["sessionId"]})
        )?["json"],
        json!({"answer":1})
    );
    manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"next JSON"}),
    )?;
    assert_eq!(
        manager
            .wait(
                &fixture.owner.session_id,
                child["sessionId"].as_str().unwrap(),
                Some(5000)
            )
            .await?["json"],
        json!({"answer":2})
    );
    assert!(declares_result(&fixture.probe.requests.lock().unwrap()[1]));
    manager.shutdown().await?;
    fixture.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_result_retry_timeout_returns_only_latest_stream_fragment() -> Result<()> {
    let mut fixture = Fixture::new(vec![
        Step::StreamSubmit(
            "first invalid streamed reply".into(),
            json!({"answer":"wrong"}),
        ),
        Step::StreamThenHang("retry fragment".into()),
        Step::Text("continued answer".into()),
    ])?;
    let child = fixture.manager.start(
        &fixture.owner,
        json!({"task":"JSON answer","output_schema":schema(),"timeout":300}),
    )?;
    fixture.entered().await?;
    let result = fixture.wait(&child).await?;
    assert_eq!(result["status"], "timed_out", "{result}");
    assert_eq!(result["text"], "retry fragment");
    assert_eq!(fixture.probe.requests.lock().unwrap().len(), 2);
    let session = child["sessionId"].as_str().unwrap();
    let messages = fixture.store.messages(session)?;
    assert!(
        messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.content == "first invalid streamed reply")
    );
    assert!(
        !messages
            .iter()
            .any(|m| m.content.contains("retry fragment"))
    );
    let run = fixture.store.runs(session)?.into_iter().last().unwrap();
    let (metadata, bytes) = fixture
        .store
        .attachment(session, &format!("bubblepaw-agent-failure-v1:{}", run.id))?
        .expect("timeout metadata");
    assert!(!metadata.contains("retry fragment"));
    assert!(bytes.is_empty());
    assert_eq!(
        serde_json::from_str::<Value>(&metadata)?["text"],
        "first invalid streamed reply"
    );
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":session,"text":"continue","output_schema":null}),
    )?;
    assert_eq!(fixture.wait(&child).await?["text"], "continued answer");
    fixture.shutdown().await?;
    Ok(())
}
