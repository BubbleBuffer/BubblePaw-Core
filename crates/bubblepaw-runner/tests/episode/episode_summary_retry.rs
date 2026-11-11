//! Synthetic providers exercise the production side-call and shared fork paths.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Message, MessageOrigin, ModelRequest, ModelResponse, Provider,
    ProviderError, ReasoningEffort, Role, TextCallback, ToolCall, ToolDefinition, Usage,
};
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, ClassicalInput, EpisodeFork, EpisodeModelConfig, EpisodeModels,
        HttpEpisodeModels, SummaryInput,
    },
    model_roles::ModelRoles,
    provider::ProviderRegistry,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

#[path = "episode_summary_retry/fixture.rs"]
mod fixture;
use fixture::*;

#[tokio::test]
async fn fresh_summary_chapter_and_classical_recover_reasoning_only_once() -> Result<()> {
    for kind in ["summary", "chapter", "classical"] {
        for succeeds in [true, false] {
            let (provider, models) = fixture(vec![
                response(" \n\t "),
                response(if succeeds { SUMMARY } else { "" }),
            ])?;
            let result = match kind {
                "summary" => models
                    .summarize(summary())
                    .await
                    .map(|r| (r.summary, r.usage)),
                "chapter" => models
                    .close_chapter(chapter())
                    .await
                    .map(|r| (r.summary, r.usage)),
                _ => models
                    .summarize_classical(ClassicalInput {
                        session_id: "session".into(),
                        previous_summary: None,
                        latest_user_request: Some("Fix the parser".into()),
                        span_text: "The parser check passed.".into(),
                    })
                    .await
                    .map(|r| (r.summary, r.usage)),
            };
            if succeeds {
                let (text, usage) = result?;
                assert!(text.contains("The parser fix passed"));
                assert_usage(&usage, 2);
            } else {
                let error = result.unwrap_err();
                assert!(error.to_string().contains("empty_output"), "{error:#}");
                assert_usage(
                    ProviderError::of(&error).unwrap().usage.as_ref().unwrap(),
                    2,
                );
            }
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_nudge(&requests[0], &requests[1]);
            assert!(
                requests
                    .iter()
                    .all(|r| r.tools.is_empty() && r.output_schema.is_none())
            );
            if kind == "summary" {
                assert!(requests[0].messages[0].content.ends_with(
                    "END OF CURRENT EPISODE. Summarize the episode above. Do not continue it and do not call tools."
                ));
                assert!(requests[0].messages[0].content.contains("ASSISTANT: Done;"));
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn shared_fork_recovery_preserves_main_prefix_and_bills_both_answers() -> Result<()> {
    for chapter_call in [false, true] {
        for native in [false, true] {
            for succeeds in [true, false] {
                let final_response = if native && succeeds {
                    let mut result = response("");
                    result.tool_calls.push(ToolCall {
                        id: "memory-return".into(),
                        name: "call_tool".into(),
                        arguments: json!({
                            "name": if chapter_call { "submit_chapter" } else { "submit_summary" },
                            "arguments": {"summary": SUMMARY},
                        }),
                    });
                    result
                } else {
                    response(if succeeds { SUMMARY } else { "" })
                };
                let (provider, models) = fixture(vec![response(""), final_response])?;
                let captured = fork(provider.clone(), native);
                let original = captured.request.clone();
                let result = if chapter_call {
                    models
                        .close_chapter_fork(chapter(), captured)
                        .await
                        .map(|r| (r.summary, r.usage))
                } else {
                    models
                        .summarize_fork(summary(), captured)
                        .await
                        .map(|r| (r.summary, r.usage))
                };
                if succeeds {
                    let (text, usage) = result?;
                    assert!(text.contains("The parser fix passed"));
                    assert_usage(&usage, 2);
                } else {
                    let error = result.unwrap_err();
                    assert!(error.to_string().contains("empty_output"), "{error:#}");
                    assert_usage(
                        ProviderError::of(&error).unwrap().usage.as_ref().unwrap(),
                        2,
                    );
                }
                let requests = provider.requests.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert_nudge(&requests[0], &requests[1]);
                assert_eq!(requests[0].system_prompt, original.system_prompt);
                assert_eq!(requests[0].messages[..2], original.messages);
                assert_eq!(
                    serde_json::to_value(&requests[0].tools)?,
                    serde_json::to_value(&original.tools)?
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn fork_empty_recovery_obeys_context_and_byte_admission_and_retains_usage() -> Result<()> {
    for chapter_call in [false, true] {
        for byte_limit in [false, true] {
            let (provider, models) = fixture(vec![response("")])?;
            let mut captured = fork(provider.clone(), false);
            if byte_limit {
                // Measure a completed first request, then set a byte cap allowing
                // that request but no appended retry control.
                let (probe, probe_models) = fixture(vec![response(SUMMARY)])?;
                let probe_fork = fork(probe.clone(), false);
                if chapter_call {
                    probe_models
                        .close_chapter_fork(chapter(), probe_fork)
                        .await?;
                } else {
                    probe_models.summarize_fork(summary(), probe_fork).await?;
                }
                captured.max_request_bytes =
                    bubblepaw_core::prepared_request_bytes(&probe.requests.lock().unwrap()[0])?;
            } else {
                captured.context_window = Some(100);
            }
            let error = if chapter_call {
                models
                    .close_chapter_fork(chapter(), captured)
                    .await
                    .unwrap_err()
            } else {
                models
                    .summarize_fork(summary(), captured)
                    .await
                    .unwrap_err()
            };
            assert!(
                error.to_string().contains(if byte_limit {
                    "request_byte_limit"
                } else {
                    "context_window_limit"
                }),
                "{error:#}"
            );
            assert_usage(
                ProviderError::of(&error).unwrap().usage.as_ref().unwrap(),
                1,
            );
            assert_eq!(provider.requests.lock().unwrap().len(), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn fork_tool_refusal_and_empty_recovery_have_separate_single_retry_bounds() -> Result<()> {
    let mut tool = response("");
    tool.tool_calls.push(ToolCall {
        id: "attempted-tool".into(),
        name: "js".into(),
        arguments: json!({"code":"throw 1"}),
    });
    for empty_first in [false, true] {
        for succeeds in [false, true] {
            let replies = if empty_first {
                vec![
                    response(""),
                    tool.clone(),
                    if succeeds {
                        response(SUMMARY)
                    } else {
                        tool.clone()
                    },
                ]
            } else {
                vec![
                    tool.clone(),
                    response(""),
                    response(if succeeds { SUMMARY } else { "" }),
                ]
            };
            let (provider, models) = fixture(replies)?;
            let result = models
                .summarize_fork(summary(), fork(provider.clone(), false))
                .await;
            if succeeds {
                assert_usage(&result?.usage, 3);
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains(if empty_first {
                        "tool_call_returned"
                    } else {
                        "empty_output"
                    }),
                    "{error:#}"
                );
                assert_usage(
                    ProviderError::of(&error).unwrap().usage.as_ref().unwrap(),
                    3,
                );
            }
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            let final_request = requests.last().unwrap();
            assert_eq!(
                final_request
                    .messages
                    .iter()
                    .filter(|m| m.content.starts_with(RETRY))
                    .count(),
                1
            );
            let refusals: Vec<_> = final_request
                .messages
                .iter()
                .filter(|m| m.role == Role::Tool)
                .collect();
            assert_eq!(refusals.len(), 1);
            assert!(refusals[0].is_error);
            assert_eq!(refusals[0].tool_call_id.as_deref(), Some("attempted-tool"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn empty_output_includes_bounded_visible_reasoning_from_native_states() -> Result<()> {
    let reasoning = format!("{}DO_NOT_INCLUDE_TAIL", "猫".repeat(500));
    let states = [
        json!({"reasoning_content": reasoning}),
        json!({"adapter":"chat-completions", "message_fields":{"reasoning_content": reasoning}}),
        json!({"adapter":"chat-completions", "message_fields":{"reasoning": reasoning}}),
        json!({"adapter":"chat-completions", "rounds":[
            {"message_fields":{"reasoning_content": reasoning}}
        ]}),
        json!({"items":[{"type":"reasoning", "summary":[{"text":reasoning}]}]}),
        json!({"content":[{"type":"thinking", "thinking":reasoning, "signature":"OPAQUE_SIGNATURE"}]}),
    ];
    for state in states {
        for kind in ["summary", "chapter", "classical", "fork"] {
            let mut reply = response("");
            reply.provider_state = Some(state.clone());
            let (provider, models) = fixture(vec![reply.clone(), reply])?;
            let error = match kind {
                "summary" => models.summarize(summary()).await.unwrap_err(),
                "chapter" => models.close_chapter(chapter()).await.unwrap_err(),
                "classical" => models
                    .summarize_classical(ClassicalInput {
                        session_id: "session".into(),
                        previous_summary: None,
                        latest_user_request: None,
                        span_text: "Synthetic transcript".into(),
                    })
                    .await
                    .unwrap_err(),
                _ => models
                    .summarize_fork(summary(), fork(provider.clone(), false))
                    .await
                    .unwrap_err(),
            };
            let detail = error.to_string();
            assert!(detail.contains("empty_output"), "{detail}");
            let preview = detail.split_once("reasoning preview: ").unwrap().1;
            assert_eq!(preview.chars().count(), 500);
            assert_eq!(preview, "猫".repeat(500));
            assert!(!detail.contains("DO_NOT_INCLUDE_TAIL"));
            assert!(!detail.contains("OPAQUE_SIGNATURE"));
            assert_eq!(provider.requests.lock().unwrap().len(), 2);
            assert_usage(
                ProviderError::of(&error).unwrap().usage.as_ref().unwrap(),
                2,
            );
        }
    }
    let mut reply = response("");
    reply.provider_state =
        Some(json!({"content":[{"type":"redacted_thinking", "data":"OPAQUE_DATA"}]}));
    let (_, models) = fixture(vec![reply.clone(), reply])?;
    let detail = models.summarize(summary()).await.unwrap_err().to_string();
    assert!(detail.contains("empty_output"));
    assert!(!detail.contains("reasoning preview"));
    assert!(!detail.contains("OPAQUE_DATA"));
    Ok(())
}
