use super::*;

fn synthetic_sse(content: Vec<Value>, stop: &str) -> RecordedStep {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"synthetic-message","type":"message","role":"assistant","content":[],"usage":{"input_tokens":20,"output_tokens":0}}}),
    ];
    for (index, mut block) in content.into_iter().enumerate() {
        let input = if block["type"] == "tool_use" {
            let input = block["input"].take();
            block["input"] = json!({});
            Some(input)
        } else {
            None
        };
        events.push(json!({"type":"content_block_start","index":index,"content_block":block}));
        if let Some(input) = input {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(
        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":10}}),
    );
    events.push(json!({"type":"message_stop"}));
    RecordedStep {
        status: StatusCode::OK,
        response: events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
            .into_bytes(),
        request: None,
    }
}

/// Synthetic smoke for the current submission contract. The parent module
/// separately preserves recorded signed history before synthetic recovery.
#[tokio::test]
async fn synthetic_research_child_runs_two_sequential_tool_rounds() -> Result<()> {
    let tool =
        |id: &str, code: &str| json!({"type":"tool_use","id":id,"name":"js","input":{"code":code}});
    let steps = vec![
        synthetic_sse(
            vec![
                json!({"type":"thinking","thinking":"Find the source.","signature":"synthetic-signature-1"}),
                tool(
                    "search",
                    "const hits=await tools.web_search({query:'recorded fixture answer'}); await tools.web_search({query:'recorded fixture answer'}); state.source=hits.results[0].id; return {id:state.source};",
                ),
            ],
            "tool_use",
        ),
        synthetic_sse(
            vec![
                json!({"type":"thinking","thinking":"Read the fetched evidence.","signature":"synthetic-signature-2"}),
                tool(
                    "fetch",
                    "const page=await tools.web_fetch({id:state.source}); await tools.web_fetch({id:state.source}); return page;",
                ),
            ],
            "tool_use",
        ),
        submission(
            json!({"findings":[{"text":"The answer is violet.","source_ids":["s1"]}],"gaps":[],"usable":true}),
        ),
    ];
    run_research_child(
        Transport::Replay(Arc::new(steps)),
        "offline-fixture-key".into(),
        ProbeCase::TwoRounds,
    )
    .await
}

#[tokio::test]
async fn synthetic_research_child_exhausts_budget_in_first_round() -> Result<()> {
    let steps = vec![
        synthetic_sse(
            vec![
                json!({"type":"thinking","thinking":"One outer JS plus five nested host calls uses all six calls.","signature":"synthetic-budget-signature"}),
                json!({"type":"tool_use","id":"all-evidence","name":"js","input":{"code":"const hits=await tools.web_search({query:'recorded fixture answer'}); state.source=hits.results[0].id; const page=await tools.web_fetch({id:state.source}); for(let n=0;n<3;n++) await tools.web_fetch({id:state.source}); return page;"}}),
            ],
            "tool_use",
        ),
        submission(
            json!({"findings":[{"text":"The answer is violet.","source_ids":["s1"]}],"gaps":[],"usable":true}),
        ),
    ];
    run_research_child(
        Transport::Replay(Arc::new(steps)),
        "offline-fixture-key".into(),
        ProbeCase::FirstRoundBudget,
    )
    .await
}

pub(super) fn submission(answer: Value) -> RecordedStep {
    synthetic_sse(
        vec![
            json!({"type":"tool_use","id":"submit-result","name":"js","input":{"code":format!("await tools.submit_result({answer});")}}),
        ],
        "tool_use",
    )
}
