//! Episode source rendering and validation regressions.
use super::*;
use serde_json::json;
fn tool(content: String) -> Message {
    serde_json::from_value(
        json!({"id":"source-id", "session_id":"session", "role":"tool",
        "tool_call_id":"call-id", "content":content, "created_at":"2026-10-17T00:00:00Z"}),
    )
    .unwrap()
}
#[test]
fn default_unlimited_evidence_retains_middle_facts_and_explicit_budget_stays_lossy() {
    assert_eq!(EpisodeConfig::default().tool_evidence_tokens, 0);
    let result = tool(format!(
        "HEAD {} BURIED_ARBITRARY_FACT=violet-bridge-47 {} TAIL",
        "padding ".repeat(5000),
        "padding ".repeat(5000)
    ));
    let mut assistant = tool("Read the exact supplied evidence".into());
    assistant.id = "source-call-message".into();
    assistant.role = Role::Assistant;
    assistant.tool_call_id = None;
    assistant.provider_state = Some(json!({"reasoning":"RAW_PRIVATE_REASONING"}));
    assistant.tool_calls.push(bubblepaw_core::ToolCall {
        id: "call-id".into(),
        name: "exec".into(),
        arguments: json!({"code":format!(
            "HEAD {} BURIED_ARGUMENT=amber-92 {} TAIL",
            "padding ".repeat(5000), "padding ".repeat(5000)
        )}),
    });
    let arguments = assistant.tool_calls[0].arguments.to_string();
    let messages = vec![assistant, result];
    let original = serde_json::to_vec(&messages).unwrap();
    let full = render_episode(&messages);
    assert!(full.contains(&messages[1].content));
    assert!(full.contains(&arguments));
    assert!(full.contains("BURIED_ARBITRARY_FACT=violet-bridge-47"));
    assert!(full.contains("BURIED_ARGUMENT=amber-92"));
    assert!(!full.contains("excerpt clipped"));
    assert!(!full.contains("RAW_PRIVATE_REASONING"));

    let bounded = render_episode_with_budget(&messages, 120);
    assert!(bounded.contains("excerpt clipped"));
    assert!(!bounded.contains("BURIED_ARBITRARY_FACT"));
    assert!(!bounded.contains("BURIED_ARGUMENT"));
    assert!(bounded.contains("Tool call ID: call-id"));
    assert!(bounded.contains("Tool call call-id (exec)"));
    assert!(text_tokens(&bounded) < text_tokens(&full) / 4);
    assert!(text_tokens(&tool_evidence(&messages[1], 60)) <= 60);
    assert_eq!(serde_json::to_vec(&messages).unwrap(), original);
}
#[test]
fn tool_evidence_keeps_short_results_and_bounded_head_tail_failures() {
    let short = tool("exact small result".into());
    assert!(render_episode(std::slice::from_ref(&short)).contains(&short.content));
    let mut failed = tool(format!(
        "START_DIAGNOSTIC {} END_FAILURE_file.rs:12",
        " output".repeat(5000)
    ));
    failed.is_error = true;
    let original = serde_json::to_vec(&failed).unwrap();
    let rendered = render_episode_with_budget(std::slice::from_ref(&failed), 120);
    assert!(rendered.contains("source-id Tool"));
    assert!(rendered.contains("Tool call ID: call-id"));
    assert!(rendered.contains("chars, error]"));
    assert!(rendered.contains("START_DIAGNOSTIC"));
    assert!(rendered.contains("END_FAILURE_file.rs:12"));
    assert!(text_tokens(&tool_evidence(&failed, 120)) <= 120);
    assert_eq!(serde_json::to_vec(&failed).unwrap(), original);
}
#[test]
fn structured_errors_are_retained_without_inferring_failure_from_prose() {
    for exit in [1, -1, 123] {
        let output = tool(
            json!({"results":[{"status":"exited","exitCode":exit,
            "stdout":" padding".repeat(2000),"stderr":"FAILED at file.rs:12"}]})
            .to_string(),
        );
        let rendered = render_episode_with_budget(&[output], 300);
        assert!(rendered.contains("chars, error]"));
        assert!(rendered.contains("exitCode="));
        assert!(rendered.contains("file.rs:12"));
    }
    let pending = tool(
        json!({"status":"running", "exitCode":0, "error":null,
        "stdout":"error failed exception".repeat(1000)})
        .to_string(),
    );
    assert!(!tool_result_failed(&pending));
    let notice = tool(format!(
        "{}\n[---]\nstate.error added",
        json!({"exitCode":0})
    ));
    assert!(!tool_result_failed(&notice));
}
#[test]
fn diagnostic_in_the_middle_of_large_json_is_preserved_as_structured_evidence() {
    let mut result = tool(format!(
        "{{\"prefix\":\"{}\",\"stderr\":\"MIDDLE_FAILURE file.rs:28\",\"suffix\":\"{}\"}}",
        "padding ".repeat(3000),
        "padding ".repeat(3000)
    ));
    result.is_error = true;
    let evidence = tool_evidence(&result, 120);
    assert!(evidence.contains("$.stderr="));
    assert!(evidence.contains("MIDDLE_FAILURE file.rs:28"));
    assert!(text_tokens(&evidence) <= 120);
}

#[test]
fn visible_text_is_retained_while_raw_reasoning_and_large_arguments_are_projected() {
    let mut assistant = tool(String::new());
    assistant.role = Role::Assistant;
    assistant.tool_call_id = None;
    assistant.content = "visible exact decision".into();
    assistant.provider_state = Some(json!({"reasoning":"RAW_PRIVATE_REASONING"}));
    assistant.tool_calls.push(bubblepaw_core::ToolCall {
        id: "source-call".into(),
        name: "exec".into(),
        arguments: json!({"code":"return value;".repeat(2000)}),
    });
    let original = serde_json::to_vec(&assistant).unwrap();
    let rendered = render_episode_with_budget(std::slice::from_ref(&assistant), 100);
    assert!(rendered.contains("visible exact decision"));
    assert!(rendered.contains("Tool call source-call (exec)"));
    assert!(rendered.contains("excerpt clipped"));
    assert!(!rendered.contains("RAW_PRIVATE_REASONING"));
    assert_eq!(serde_json::to_vec(&assistant).unwrap(), original);
}
#[test]
fn many_results_share_budget_keep_all_ids_and_do_not_include_raw_reasoning() {
    let messages: Vec<_> = (0..20)
        .map(|index| {
            let mut result = tool(
                json!({"status":"exited", "exitCode":2,
            "stdout":"large output ".repeat(1000),"stderr":"exact failure"})
                .to_string(),
            );
            result.id = format!("source-{index}");
            result.tool_call_id = Some(format!("call-{index}"));
            result
        })
        .collect();
    let original = serde_json::to_vec(&messages).unwrap();
    let rendered = render_episode_with_budget(&messages, 1000);
    for index in 0..20 {
        assert!(rendered.contains(&format!("source-{index} Tool")));
        assert!(rendered.contains(&format!("Tool call ID: call-{index}")));
    }
    assert!(rendered.contains("exact failure"));
    assert_eq!(serde_json::to_vec(&messages).unwrap(), original);
}
#[test]
fn rendered_transcript_echoed_as_a_summary_is_not_memory() {
    let call: Message = serde_json::from_value(json!({"id":"assistant-id",
        "session_id":"session", "role":"assistant", "content":"Reading.",
        "tool_calls":[{"id":"call-id","name":"js","arguments":{"code":"1"}}],
        "created_at":"2026-10-17T00:00:00Z"}))
    .unwrap();
    for rendered in [
        render_episode(std::slice::from_ref(&call)),
        render_episode(&[tool("plain result".into())]),
    ] {
        assert_eq!(
            crate::episode_models::summary_output_reason("summary", &rendered),
            Some("transcript_output"),
            "{rendered}"
        );
    }
}
fn stored(ordinal: usize, summary: &str) -> ContextEpisode {
    serde_json::from_value(json!({"id":format!("episode-{ordinal}"),
        "session_id":"session", "ordinal":ordinal, "first_message_id":"a",
        "last_message_id":"b", "source_tokens":1, "cap":800,
        "origin_request_id":null, "title":"t", "summary":summary, "model":"m",
        "usage":null, "created_at":"2026-10-19T00:00:00Z", "chapter_id":"c"}))
    .unwrap()
}
#[test]
fn invalid_stored_summary_is_never_the_previous_episode() {
    let valid = "# Step 1\nAdded camel_to_snake; 5 tests OK.";
    let flash = [
        "Step 2 verified.\n\nTool call call_00_x (js) arguments:\n{\"code\":\"1\"}",
        "Reading the file.\n\n<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"js\">",
    ];
    let episodes = [stored(1, valid), stored(2, flash[0]), stored(3, flash[1])];
    assert_eq!(last_valid_summary(&episodes).as_deref(), Some(valid));
    assert_eq!(last_valid_summary(&episodes[1..]), None);
    assert_eq!(last_valid_summary(&[]), None);
    let later = [stored(1, flash[0]), stored(2, valid)];
    assert_eq!(last_valid_summary(&later).as_deref(), Some(valid));
}
