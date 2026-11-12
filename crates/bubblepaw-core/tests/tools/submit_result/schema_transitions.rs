//! Returning to the initial contract must supersede any later durable notice.
use super::*;

fn string_options() -> RunOptions {
    RunOptions {
        output_schema: Some(
            JsonSchema::new(
                "string_answer",
                json!({
                    "type":"object","properties":{"answer":{"type":"string"}},
                    "required":["answer"],"additionalProperties":false
                }),
            )
            .unwrap(),
        ),
        ..Default::default()
    }
}
fn notice<'a>(request: &'a ModelRequest, run: &Run) -> &'a str {
    let marker = format!("<harness kind=\"result_schema:{}\">", run.id);
    request
        .messages
        .iter()
        .find_map(|message| {
            message
                .content
                .split_once(&marker)?
                .1
                .split_once("</harness>")
                .map(|(body, _)| body)
        })
        .expect("the current run must supersede the previous visible result contract")
}
fn definition(body: &str) -> Result<ToolDefinition> {
    let (encoded, definition) = body
        .lines()
        .find_map(|line| {
            serde_json::from_str::<ToolDefinition>(line)
                .ok()
                .map(|definition| (line, definition))
        })
        .expect("the notice must include the complete current result definition");
    assert_eq!(encoded, serde_json::to_string(&definition)?);
    assert_eq!(definition.name, "submit_result");
    Ok(definition)
}
fn stable_prefix(requests: &[ModelRequest]) -> Result<()> {
    let prefix = |request: &ModelRequest| {
        serde_json::to_vec(&json!({
            "system":request.system_prompt,"tools":request.tools
        }))
    };
    let initial = prefix(&requests[0])?;
    for request in requests {
        assert_eq!(prefix(request)?, initial);
        assert!(request.output_schema.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn returning_to_the_initial_schema_supersedes_the_intervening_schema_notice() -> Result<()> {
    let f = fixture(vec![
        submit(json!({"answer":1})),
        submit(json!({"answer":"intervening contract"})),
        submit(json!({"answer":3})),
    ])?;
    let initial = f
        .harness
        .prompt(&f.session, "initial integer result", options())
        .await?;
    assert_eq!(initial.status, "completed", "{:?}", initial.error);
    let changed = f
        .harness
        .prompt(&f.session, "string result", string_options())
        .await?;
    assert_eq!(changed.status, "completed", "{:?}", changed.error);
    let restored = f
        .harness
        .prompt(&f.session, "integer result again", options())
        .await?;
    assert_eq!(restored.status, "completed", "{:?}", restored.error);
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    stable_prefix(&requests)?;
    assert_eq!(
        definition(notice(&requests[1], &changed))?.input_schema,
        string_options().output_schema.unwrap().schema
    );
    assert_eq!(
        definition(notice(&requests[2], &restored))?.input_schema,
        options().output_schema.unwrap().schema
    );
    assert_eq!(
        requests[2].tools.last().unwrap().input_schema,
        options().output_schema.unwrap().schema
    );
    assert_eq!(restored.text.as_deref(), Some(r#"{"answer":3}"#));
    Ok(())
}

#[tokio::test]
async fn returning_to_an_ordinary_run_supersedes_the_intervening_required_result_notice()
-> Result<()> {
    let f = fixture(vec![
        text("initial ordinary reply"),
        submit(json!({"answer":"required temporarily"})),
        text("ordinary reply again"),
    ])?;
    let initial = f
        .harness
        .prompt(&f.session, "ordinary task", RunOptions::default())
        .await?;
    assert_eq!(initial.status, "completed", "{:?}", initial.error);
    let required = f
        .harness
        .prompt(&f.session, "required result", string_options())
        .await?;
    assert_eq!(required.status, "completed", "{:?}", required.error);
    let plain = f
        .harness
        .prompt(&f.session, "ordinary task again", RunOptions::default())
        .await?;
    assert_eq!(plain.status, "completed", "{:?}", plain.error);
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    stable_prefix(&requests)?;
    assert_eq!(
        definition(notice(&requests[1], &required))?.input_schema,
        string_options().output_schema.unwrap().schema
    );
    let disabled = notice(&requests[2], &plain);
    assert!(disabled.contains("no output schema"));
    assert!(disabled.contains("submit_result is unavailable"));
    assert!(requests.iter().all(|request| {
        !request
            .tools
            .iter()
            .any(|tool| tool.name == "submit_result")
    }));
    assert_eq!(plain.text.as_deref(), Some("ordinary reply again"));
    Ok(())
}
