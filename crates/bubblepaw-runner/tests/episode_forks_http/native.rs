//! Native forks keep a delegated main task's tools and use the already declared
//! call_tool entrypoint for branch-local memory delivery, without wire schemas.
use super::*;
use bubblepaw_core::{ContextSubjectChapter, JsonSchema, SubjectChapterStatus};
use bubblepaw_runner::episode_models::ChapterInput;

pub(super) fn main_schema() -> JsonSchema {
    JsonSchema::new(
        "delegated_answer",
        json!({
            "type":"object", "properties":{"done":{"type":"boolean"}},
            "required":["done"], "additionalProperties":false
        }),
    )
    .unwrap()
}

pub(super) fn native_request(structured: bool) -> ModelRequest {
    let mut parent = request();
    parent.tools.push(ToolDefinition {
        name: "call_tool".into(),
        description: "Frozen native dispatcher".into(),
        input_schema: json!({"type":"object","properties":{"name":{"type":"string"},"arguments":{"type":"object"}},"required":["name","arguments"],"additionalProperties":false}),
        read_only: true,
        approval: Default::default(),
    });
    if structured {
        parent.tools.push(ToolDefinition {
            name: "submit_result".into(),
            description: "Submit the main task's result".into(),
            input_schema: main_schema().schema,
            read_only: true,
            approval: Default::default(),
        });
    }
    // A legacy caller may still set this field; neither provider nor fork may
    // turn it into a response-format constraint.
    parent.output_schema = structured.then(main_schema);
    parent.messages[0].origin = Some(MessageOrigin::ChildTask);
    parent
}

pub(super) fn delivery(tool: &str, arguments: Value) -> Value {
    json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":"discard this wrapper text","tool_calls":[{"id":"branch-return","type":"function","function":{"name":tool,"arguments":arguments.to_string()}}]}}],"usage":{"prompt_tokens":501,"completion_tokens":23,"prompt_tokens_details":{"cached_tokens":400}}})
}

pub(super) fn submit(target: &str, summary: &str) -> Value {
    delivery(
        "call_tool",
        json!({"name":target,"arguments":{"summary":summary}}),
    )
}

#[tokio::test]
async fn native_summary_preserves_tools_and_prefix_without_schema_and_extracts_only_memory()
-> Result<()> {
    for structured in [false, true] {
        let text = "# Native memory\nExact evidence [source-t] [call-exact].\nOpen: repair";
        let f = fixture(vec![
            reply("{\"done\":false}"),
            submit("submit_summary", text),
            reply("{\"done\":true}"),
        ])
        .await?;
        let parent = native_request(structured);
        let before = serde_json::to_vec(&parent)?;
        f.registry
            .generate(parent.clone(), Arc::new(|_| {}), CancellationToken::new())
            .await?;
        let result = models(f.registry.clone(), None)?
            .summarize_fork(input(), fork(f.registry.clone(), parent.clone()))
            .await?;
        assert_eq!(result.title, "Native memory");
        assert_eq!(result.summary, text);
        assert_eq!(result.usage.cache_read_tokens, 400);
        assert_eq!(serde_json::to_vec(&parent)?, before);
        f.registry
            .generate(parent, Arc::new(|_| {}), CancellationToken::new())
            .await?;
        let rows = f.wire.requests.lock().unwrap();
        let main = &rows[0].1;
        let branch = &rows[1].1;
        assert_eq!(
            serde_json::to_vec(&branch["tools"])?,
            serde_json::to_vec(&main["tools"])?
        );
        assert!(main.get("response_format").is_none());
        assert!(branch.get("response_format").is_none());
        let prefix = main["messages"].as_array().unwrap();
        assert_eq!(
            serde_json::to_vec(&branch["messages"].as_array().unwrap()[..prefix.len()])?,
            serde_json::to_vec(prefix)?
        );
        let control = branch["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(control.contains("call_tool({name:'submit_summary',arguments:{summary:'text'}})"));
        assert!(control.contains("never call it directly, through call_tool or through js"));
        assert!(!branch["tools"].to_string().contains("submit_summary"));
        assert_eq!(&rows[2].1, main);
    }
    Ok(())
}

#[tokio::test]
async fn native_chapter_preserves_tools_without_schema_and_uses_existing_related_parser()
-> Result<()> {
    let text = "# Parser continuation\nExact parser evidence [source-t].\nRelated: earlier (task: \"resume parser repair\")";
    let f = fixture(vec![
        reply("{\"done\":false}"),
        submit("submit_chapter", text),
    ])
    .await?;
    let mut parent = native_request(true);
    parent.reasoning_effort = Some(ReasoningEffort::Low);
    f.registry
        .generate(parent.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    let result = models(f.registry.clone(), None)?
        .close_chapter_fork(
            ChapterInput {
                session_id: "source-session".into(),
                episodes: vec!["# Episode\nParser evidence [source-t].\nOpen: repair".into()],
                previous_chapters: vec![ContextSubjectChapter {
                    id: "earlier".into(),
                    session_id: "source-session".into(),
                    ordinal: 0,
                    first_episode_ordinal: 0,
                    last_episode_ordinal: 0,
                    status: SubjectChapterStatus::Closed,
                    description: Some("Parser repair".into()),
                    summary: Some("Earlier parser state".into()),
                    related_ids: vec![],
                    related_evidence: vec![],
                    model: None,
                    usage: None,
                    reasoning: None,
                }],
                typed_user_texts: vec![],
                task_instructions: vec!["resume parser repair".into()],
            },
            fork(f.registry.clone(), parent),
        )
        .await?;
    assert_eq!(result.description, "Parser continuation");
    assert_eq!(result.summary, "Exact parser evidence [source-t].");
    assert_eq!(result.related_ids, ["earlier"]);
    assert_eq!(result.related_evidence.len(), 1);
    let rows = f.wire.requests.lock().unwrap();
    let (main, branch) = (&rows[0].1, &rows[1].1);
    assert_eq!(branch["tools"], main["tools"]);
    assert!(main.get("response_format").is_none());
    assert!(branch.get("response_format").is_none());
    let prefix = main["messages"].as_array().unwrap();
    assert_eq!(
        &branch["messages"].as_array().unwrap()[..prefix.len()],
        prefix
    );
    assert!(
        branch["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("call_tool({name:'submit_chapter',arguments:{summary:'text'}})")
    );
    assert!(!branch["tools"].to_string().contains("submit_chapter"));
    Ok(())
}

#[tokio::test]
async fn native_wrong_or_malformed_delivery_is_refused_once_before_correct_return() -> Result<()> {
    let mut multiple = submit("submit_summary", "must not accept one of several calls");
    let mut duplicate = multiple["choices"][0]["message"]["tool_calls"][0].clone();
    duplicate["id"] = "branch-return-extra".into();
    multiple["choices"][0]["message"]["tool_calls"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    let wrong = [
        delivery("submit_result", json!({"done":true})),
        delivery(
            "call_tool",
            json!({"name":"submit_result","arguments":{"done":true}}),
        ),
        delivery(
            "js",
            json!({"code":"await tools.submit_result({done:true})"}),
        ),
        submit("submit_chapter", "wrong target"),
        delivery(
            "call_tool",
            json!({"name":"submit_summary","arguments":{"summary":42}}),
        ),
        delivery(
            "call_tool",
            json!({"name":"submit_summary","arguments":{"summary":"memory","extra":true}}),
        ),
        delivery(
            "call_tool",
            json!({"name":"submit_summary","arguments":null}),
        ),
        multiple,
    ];
    for returned in wrong {
        let text = "# Retry memory\nCorrect extracted summary.\nOpen: none";
        let f = fixture(vec![returned, submit("submit_summary", text)]).await?;
        let result = models(f.registry.clone(), None)?
            .summarize_fork(input(), fork(f.registry.clone(), native_request(true)))
            .await?;
        assert_eq!(result.summary, text);
        assert_eq!(result.usage.input_tokens, 1002);
        let rows = f.wire.requests.lock().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].1.get("response_format").is_none());
        assert!(rows[1].1.get("response_format").is_none());
        assert_eq!(rows[0].1["tools"], rows[1].1["tools"]);
        let first = rows[0].1["messages"].as_array().unwrap();
        let retry = rows[1].1["messages"].as_array().unwrap();
        assert_eq!(&retry[..first.len()], first);
        assert!(
            retry.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("submit_summary")
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_wrong_target_exhausts_exactly_one_refusal_retry() -> Result<()> {
    let wrong = submit("submit_chapter", "# Wrong target\nOpen: none");
    let f = fixture(vec![wrong.clone(), wrong]).await?;
    let error = models(f.registry.clone(), None)?
        .summarize_fork(input(), fork(f.registry.clone(), native_request(true)))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tool_call_returned"));
    assert_eq!(decision_error_usage(&error).unwrap().input_tokens, 1002);
    assert_eq!(f.wire.requests.lock().unwrap().len(), 2);
    Ok(())
}

#[tokio::test]
async fn js_fork_refuses_main_result_and_synthetic_memory_attempts_inside_js() -> Result<()> {
    for code in [
        "await tools.submit_result({done:true})",
        "await tools.submit_summary({summary:'must not execute'})",
    ] {
        let wrong = delivery("js", json!({"code":code}));
        let f = fixture(vec![wrong, reply("# JS memory\nRecorded.\nOpen: none")]).await?;
        let mut parent = request();
        parent.output_schema = Some(main_schema());
        let result = models(f.registry.clone(), None)?
            .summarize_fork(input(), fork(f.registry.clone(), parent))
            .await?;
        assert_eq!(result.title, "JS memory");
        assert_eq!(result.usage.input_tokens, 1002);
        let rows = f.wire.requests.lock().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|(_, body)| body.get("response_format").is_none())
        );
        assert_eq!(rows[0].1["tools"], rows[1].1["tools"]);
        let initial = rows[0].1["messages"].as_array().unwrap();
        let retried = rows[1].1["messages"].as_array().unwrap();
        assert_eq!(&retried[..initial.len()], initial);
        assert_eq!(
            retried.last().unwrap()["content"],
            "No tools in this task; answer in text."
        );
    }
    Ok(())
}

#[tokio::test]
async fn js_fork_refuses_reserved_submission_and_rejects_other_undeclared_tools() -> Result<()> {
    for wrong in [
        delivery("submit_result", json!({"done":true})),
        delivery(
            "call_tool",
            json!({"name":"submit_result","arguments":{"done":true}}),
        ),
        submit("submit_summary", "# Wrong delivery\nOpen: none"),
    ] {
        let reserved =
            wrong["choices"][0]["message"]["tool_calls"][0]["function"]["name"] == "submit_result";
        let replies = if reserved {
            vec![wrong.clone(), wrong]
        } else {
            vec![wrong]
        };
        let f = fixture(replies).await?;
        let mut parent = request();
        parent.output_schema = Some(main_schema());
        let error = models(f.registry.clone(), None)?
            .summarize_fork(input(), fork(f.registry.clone(), parent))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(if reserved {
            "tool_call_returned"
        } else {
            "returned an unavailable tool"
        }));
        let rows = f.wire.requests.lock().unwrap();
        assert_eq!(rows.len(), if reserved { 2 } else { 1 });
        if reserved {
            assert_eq!(rows[0].1["tools"], rows[1].1["tools"]);
        }
        assert!(rows[0].1.get("response_format").is_none());
    }
    Ok(())
}

#[tokio::test]
async fn native_chapter_wrong_summary_target_exhausts_its_single_retry() -> Result<()> {
    let wrong = submit("submit_summary", "# Wrong target\nOpen: none");
    let f = fixture(vec![wrong.clone(), wrong]).await?;
    let mut parent = native_request(true);
    parent.reasoning_effort = Some(ReasoningEffort::Low);
    let error = models(f.registry.clone(), None)?
        .close_chapter_fork(
            ChapterInput {
                session_id: "source-session".into(),
                episodes: vec!["# Memory\nEvidence.\nOpen: none".into()],
                previous_chapters: vec![],
                typed_user_texts: vec![],
                task_instructions: vec![],
            },
            fork(f.registry.clone(), parent),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tool_call_returned"));
    assert_eq!(decision_error_usage(&error).unwrap().input_tokens, 1002);
    let rows = f.wire.requests.lock().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows[1].1["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("submit_chapter")
    );
    Ok(())
}

#[tokio::test]
async fn native_and_js_plain_text_accept_legacy_schema_without_publishing_it() -> Result<()> {
    for native in [false, true] {
        for legacy_schema in [false, true] {
            let f = fixture(vec![reply("# Plain memory\nRecorded.\nOpen: none")]).await?;
            let mut parent = if native {
                native_request(legacy_schema)
            } else {
                request()
            };
            parent.output_schema = legacy_schema.then(main_schema);
            let result = models(f.registry.clone(), None)?
                .summarize_fork(input(), fork(f.registry.clone(), parent))
                .await?;
            assert_eq!(result.title, "Plain memory");
            let rows = f.wire.requests.lock().unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].1.get("response_format").is_none());
        }
    }
    Ok(())
}

#[tokio::test]
async fn synthetic_delivery_still_rejects_protocol_only_memory() -> Result<()> {
    let f = fixture(vec![submit(
        "submit_summary",
        "<tool_call>execute</tool_call>",
    )])
    .await?;
    let error = models(f.registry.clone(), None)?
        .summarize_fork(input(), fork(f.registry.clone(), native_request(true)))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("tool_protocol_output"),
        "{error}"
    );
    assert_eq!(f.wire.requests.lock().unwrap().len(), 1);
    Ok(())
}
