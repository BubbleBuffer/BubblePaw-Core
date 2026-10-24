use super::*;
use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Answer {
    status: String,
    count: u32,
}

pub(super) async fn schema(kind: Kind) {
    let output_schema = JsonSchema::new("live_two_fields", json!({
        "type":"object","additionalProperties":false,
        "properties":{"status":{"type":"string","enum":["ready"]},"count":{"type":"integer","enum":[2]}},
        "required":["status","count"]
    })).unwrap();
    let Some(fixture) = Fixture::new(kind, 1, 1, false, false).await else {
        return;
    };
    let session = fixture.store.create_session("live schema").unwrap();
    let harness = fixture.harness(
        ToolRegistry::new(),
        "Call submit_result with status ready and count 2.",
        1,
    );
    let run = prompt(
        &harness,
        &session.id,
        "Run the two-field structured output check.",
        RunOptions {
            output_schema: Some(output_schema),
            ..Default::default()
        },
    )
    .await;
    assert!(
        run.status == "completed",
        "live schema response did not complete: status={} error={}",
        run.status,
        run.error.as_deref().unwrap_or("none")
    );
    assert_eq!(
        serde_json::from_str::<Answer>(run.text.as_deref().unwrap()).unwrap(),
        Answer {
            status: "ready".into(),
            count: 2
        }
    );
    {
        let requests = fixture.witness.requests.lock().unwrap();
        assert!(requests[0].output_schema.is_none());
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "submit_result")
                || requests[0]
                    .tools
                    .iter()
                    .any(|tool| tool.name == "js" && tool.description.contains("submit_result"))
        );
    }
    let stored = fixture.stored_messages(&session.id);
    assert!(
        stored
            .iter()
            .any(|message| message.role == Role::Tool && !message.is_error),
        "accepted submission not persisted"
    );
    fixture.assert_counts(1, 1);
    fixture.provider.close_session(&session.id).await.unwrap();
}
