use anyhow::Result;
use bubblepaw_core::{
    AnswerKind, CancellationToken, DecisionChoice, DecisionProvider, DecisionRequest,
    DecisionUsage, ModelRequest, Provider, Question, QuestionKind, ReasoningEffort, ScoreLevel,
};
use bubblepaw_runner::{
    episode_models::{SplitInput, SplitOption},
    model_roles::ModelRole,
    provider::{
        LlmDecisions, OpenAiDecisions, OpenCodeDecisions, OpenRouterDecisions, ProviderEndpoints,
        ProviderKeys, ProviderRegistry, ScriptedProvider,
    },
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
};

const SPLIT_QUESTION: &str = "Which of the numbered user messages starts a new subject, different from the last chapter? A new subject is a different concrete task or workstream. Clarifications, corrections, approvals, \"continue\", complaints about the agent, and carrying out the same plan are not new subjects. If none of them starts a new subject, answer none.";
fn choice_question() -> Question {
    Question {
        name: "choice".into(),
        instructions: SPLIT_QUESTION.into(),
        kind: QuestionKind::Choice {
            choices: vec![
                DecisionChoice {
                    value: "1".into(),
                    description: "Select option 1 from the state.".into(),
                },
                DecisionChoice {
                    value: "none".into(),
                    description: "No listed option applies.".into(),
                },
            ],
        },
    }
}
fn request(questions: Vec<Question>) -> DecisionRequest {
    DecisionRequest {
        model: "fixture".into(),
        input: "read the file".into(),
        questions,
        session_id: Some("s".into()),
        reasoning_effort: Default::default(),
    }
}

#[test]
fn openrouter_fixed_split_body_matches_prechange_jev_golden() {
    let input = SplitInput {
        session_id: "fixed-session".into(),
        open_chapter: "Fix the import".into(),
        options: vec![SplitOption {
            message_id: "u1".into(),
            user: "Please  inspect\n the next task".into(),
            agent_before: "Import   fixed".into(),
        }],
    };
    // Frozen pre-change Jev payload for this SplitInput, including its whitespace collapse.
    let golden = json!({"model":"typesafe/jev-1.13","state":"LAST CHAPTER:\nFix the import\n\nNEXT USER MESSAGES:\n1. (agent before: Import fixed) Please inspect the next task","questions":{"choice":{"type":"choice","instructions":SPLIT_QUESTION,"criteria":{"1":"Select option 1 from the state.","none":"No listed option applies."}}},"session_id":"fixed-session"});
    let req = DecisionRequest {
        model: "typesafe/jev-1.13".into(),
        input: format!(
            "LAST CHAPTER:\n{}\n\nNEXT USER MESSAGES:\n1. (agent before: {}) {}",
            input.open_chapter,
            input.options[0]
                .agent_before
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            input.options[0]
                .user
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        ),
        questions: vec![choice_question()],
        session_id: Some(input.session_id),
        reasoning_effort: Default::default(),
    };
    assert_eq!(
        serde_json::to_vec(&OpenRouterDecisions::body(&req)).unwrap(),
        serde_json::to_vec(&golden).unwrap()
    );
}

#[tokio::test]
async fn openrouter_three_samples_preserve_full_split_golden_and_average_usage() -> Result<()> {
    let payloads = [0.1, 0.3, 0.2].iter().map(|p| json!({
        "model":"typesafe/jev-1.13",
        "answers":{"choice":{"type":"choice","choice":"2","probabilities":{"1":p,"2":0.8-p,"none":0.2}}},
        "usage":{"input_tokens":11,"output_tokens":2,"cost":0.001}
    })).collect();
    let (base, server) = fake_server_sequence(payloads);
    let mut registry = ProviderRegistry::new("main", Arc::new(ScriptedProvider::new("unused")));
    registry.insert_decision(
        "openrouter",
        Arc::new(OpenRouterDecisions::new(
            "fixture-key",
            format!("{base}/decisions"),
        )?),
    )?;
    let state = format!(
        "LAST CHAPTER:\n# prior\nsummary\n\nNEXT USER MESSAGES:\n1. (agent before: {}tail) first request\n2. (agent before: answer) next task",
        "x".repeat(396)
    );
    let mut question = choice_question();
    if let QuestionKind::Choice { choices } = &mut question.kind {
        choices.insert(
            1,
            DecisionChoice {
                value: "2".into(),
                description: "Select option 2 from the state.".into(),
            },
        );
    }
    let role = ModelRole {
        models: vec!["openrouter/typesafe/jev-1.13".into()],
        repeats: Some(3),
        threshold: Some(0.8),
        reasoning_effort: Default::default(),
        max_output_tokens: None,
    };
    let result = role
        .decide(
            &registry,
            DecisionRequest {
                model: String::new(),
                input: state.clone(),
                questions: vec![question],
                session_id: Some("session".into()),
                reasoning_effort: Default::default(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert!(
        matches!(&result.answers[0].kind, AnswerKind::Choice {choice,probabilities} if choice=="2" && (probabilities[0].1 - 0.2).abs() < 1e-12 && (probabilities[1].1 - 0.6).abs() < 1e-12)
    );
    assert_eq!(result.usage.input_tokens, 33);
    assert_eq!(result.usage.output_tokens, 6);
    assert!((result.usage.cost_usd.unwrap() - 0.003).abs() < 1e-12);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    let golden = json!({
        "model":"typesafe/jev-1.13", "state":state,
        "questions":{"choice":{"type":"choice","instructions":SPLIT_QUESTION,"criteria":{"1":"Select option 1 from the state.","2":"Select option 2 from the state.","none":"No listed option applies."}}},
        "session_id":"session"
    });
    for (path, body) in requests {
        assert_eq!(path, "POST /decisions HTTP/1.1");
        assert_eq!(serde_json::to_vec(&body)?, serde_json::to_vec(&golden)?);
    }
    Ok(())
}

#[test]
fn provider_registration_skips_missing_keys_and_preserves_registered_adapters() -> Result<()> {
    let endpoints = ProviderEndpoints::default();
    let mut registry = ProviderRegistry::new("main", Arc::new(ScriptedProvider::new("unused")));
    endpoints.register_with_keys(&mut registry, ProviderKeys::default())?;
    assert_eq!(registry.ids(), vec!["main"]);
    assert!(registry.get_decision("openai").is_none());
    assert!(registry.get_decision("openrouter").is_none());
    endpoints.register_with_keys(
        &mut registry,
        ProviderKeys {
            opencode: Some("".into()),
            openai: Some("fixture".into()),
            openrouter: Some("fixture".into()),
        },
    )?;
    assert!(registry.get("opencode").is_none());
    assert!(registry.get("opencode-messages").is_none());
    let openai = registry.get_decision("openai").unwrap();
    let openrouter = registry.get_decision("openrouter").unwrap();
    endpoints.register_with_keys(
        &mut registry,
        ProviderKeys {
            opencode: Some("fixture".into()),
            openai: Some("replacement".into()),
            openrouter: Some("replacement".into()),
        },
    )?;
    assert!(registry.get("opencode").is_some());
    assert!(registry.get("opencode-messages").is_some());
    assert!(Arc::ptr_eq(
        &openai,
        &registry.get_decision("openai").unwrap()
    ));
    assert!(Arc::ptr_eq(
        &openrouter,
        &registry.get_decision("openrouter").unwrap()
    ));
    Ok(())
}

#[test]
fn provider_registration_reads_configured_env_names_and_skips_absent_credentials() -> Result<()> {
    let absent = format!("BUBBLEPAW_ABSENT_{}", uuid::Uuid::new_v4().simple());
    let endpoints = ProviderEndpoints {
        opencode_key_env: absent.clone(),
        opencode_messages_key_env: absent.clone(),
        openrouter_key_env: absent.clone(),
        openai_key_env: absent,
        ..Default::default()
    };
    let mut registry = ProviderRegistry::new("main", Arc::new(ScriptedProvider::new("unused")));
    endpoints.register(&mut registry)?;
    assert_eq!(registry.ids(), vec!["main"]);
    assert!(registry.get_decision("openai").is_none());
    assert!(registry.get_decision("openrouter").is_none());
    Ok(())
}

#[test]
fn provider_settings_reject_unsafe_urls_invalid_env_names_and_nonreasoning_fields() {
    for endpoints in [
        ProviderEndpoints {
            opencode_base_url: Some("http://user:secret@localhost".into()),
            ..Default::default()
        },
        ProviderEndpoints {
            openai_base_url: "https://localhost/?secret=hidden".into(),
            ..Default::default()
        },
        ProviderEndpoints {
            openrouter_decisions_url: "ftp://localhost".into(),
            ..Default::default()
        },
        ProviderEndpoints {
            opencode_reasoning_fields: json!({"tools":[]}),
            ..Default::default()
        },
        ProviderEndpoints {
            opencode_reasoning_fields: json!(null),
            ..Default::default()
        },
        ProviderEndpoints {
            opencode_key_env: "invalid=secret".into(),
            ..Default::default()
        },
    ] {
        let error = endpoints.validate().unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
}

fn fake_server(payload: Value) -> (String, std::thread::JoinHandle<(String, Value)>) {
    fake_server_body(payload.to_string())
}
fn fake_server_body(text: String) -> (String, std::thread::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || exchange(&listener, &text));
    (base, handle)
}
fn exchange(listener: &TcpListener, text: &str) -> (String, Value) {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let (header_end, length) = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (end + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    let line = String::from_utf8_lossy(&bytes[..header_end])
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let body = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",text.len(),text).unwrap();
    (line, body)
}
fn fake_server_sequence(
    payloads: Vec<Value>,
) -> (String, std::thread::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        payloads
            .iter()
            .map(|payload| exchange(&listener, &payload.to_string()))
            .collect()
    });
    (base, handle)
}

#[tokio::test]
async fn responses_llm_decision_and_normal_requests_default_to_low_reasoning() -> Result<()> {
    let event = json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"{\"safe\":true}"}]}],"usage":{"input_tokens":7,"output_tokens":1}}});
    let (base, server) = fake_server_body(format!("data: {event}\n\n"));
    let provider = bubblepaw_runner::provider::ResponsesProvider::api_key("fixture-key", base)?;
    let response = LlmDecisions::new(Arc::new(provider))
        .decide(
            request(vec![Question {
                name: "safe".into(),
                instructions: "Safe?".into(),
                kind: QuestionKind::Predicate,
            }]),
            CancellationToken::new(),
        )
        .await?;
    let (_, body) = server.join().unwrap();
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(response.usage.input_tokens, 7);
    let (base, server) = fake_server_body(format!("data: {event}\n\n"));
    let provider = bubblepaw_runner::provider::ResponsesProvider::api_key("fixture-key", base)?;
    provider
        .generate(
            ModelRequest {
                model: "fixture".into(),
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
                disabled_tools: Vec::new(),
                max_output_tokens: None,
                session_id: String::new(),
                reasoning_effort: None,
                output_schema: None,
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await?;
    let (_, body) = server.join().unwrap();
    assert_eq!(body["reasoning"]["effort"], "low");
    let (base, server) = fake_server_body(format!("data: {event}\n\n"));
    let provider = bubblepaw_runner::provider::ResponsesProvider::api_key("fixture-key", base)?
        .with_reasoning_effort("high");
    let mut req = request(vec![Question {
        name: "safe".into(),
        instructions: "Safe?".into(),
        kind: QuestionKind::Predicate,
    }]);
    req.reasoning_effort = ReasoningEffort::Medium;
    LlmDecisions::new(Arc::new(provider))
        .decide(req, CancellationToken::new())
        .await?;
    let (_, body) = server.join().unwrap();
    assert_eq!(body["reasoning"]["effort"], "medium");
    Ok(())
}

#[tokio::test]
async fn chat_effort_override_preserves_configured_thinking_and_parses_cache_usage() -> Result<()> {
    use bubblepaw_runner::provider::ChatCompletionsProvider;
    let reply = json!({"choices":[{"message":{"content":"{\"safe\":true}"}}],"usage":{"prompt_tokens":100,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":65,"cache_creation_tokens":8}}});
    let (base, server) =
        fake_server_sequence(vec![reply.clone(), reply.clone(), reply.clone(), reply]);
    let provider = ChatCompletionsProvider::api_key("fixture-key", base.clone())?
        .with_reasoning_fields(json!({"reasoning_effort":"max","thinking":{"type":"enabled"}}))?;
    for effort in [None, Some(ReasoningEffort::High)] {
        let response = provider
            .generate(
                ModelRequest {
                    model: "fixture".into(),
                    reasoning_effort: effort,
                    ..Default::default()
                },
                Arc::new(|_| {}),
                CancellationToken::new(),
            )
            .await?;
        let usage = response.usage.unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cache_read_tokens,
                usage.cache_creation_tokens
            ),
            (100, 65, 8)
        );
    }
    let mut req = request(vec![Question {
        name: "safe".into(),
        instructions: "Safe?".into(),
        kind: QuestionKind::Predicate,
    }]);
    req.reasoning_effort = ReasoningEffort::Medium;
    LlmDecisions::new(Arc::new(provider))
        .decide(req, CancellationToken::new())
        .await?;
    ChatCompletionsProvider::api_key("fixture-key", base)?
        .with_reasoning_fields(json!({}))?
        .generate(
            ModelRequest {
                model: "fixture".into(),
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await?;
    for ((path, body), (effort, configured_thinking)) in server.join().unwrap().into_iter().zip([
        ("max", true),
        ("high", true),
        ("medium", true),
        ("low", false),
    ]) {
        assert_eq!(path, "POST /chat/completions HTTP/1.1");
        assert_eq!(body["reasoning_effort"], effort);
        if configured_thinking {
            assert_eq!(body["thinking"], json!({"type":"enabled"}));
        } else {
            assert!(body.get("thinking").is_none());
        }
        for name in ["temperature", "top_p", "top_k"] {
            assert!(body.get(name).is_none());
        }
    }
    Ok(())
}
#[test]
fn chat_reasoning_configuration_rejects_disabled_values_and_accepts_enabled_forms() -> Result<()> {
    use bubblepaw_runner::provider::ChatCompletionsProvider;
    for fields in [
        json!({"reasoning_effort":"none"}),
        json!({"reasoning_effort":"disabled"}),
        json!({"reasoning_effort":4}),
        json!({"thinking":"disabled"}),
        json!({"thinking":{"type":"disabled"}}),
        json!({"thinking":false}),
    ] {
        assert!(
            ChatCompletionsProvider::api_key("fixture-key", "http://127.0.0.1:9")?
                .with_reasoning_fields(fields)
                .is_err()
        );
    }
    for fields in [
        json!({}),
        json!({"thinking":"enabled"}),
        json!({"thinking":{"type":"enabled"}}),
        json!({"thinking":{"type":"adaptive"},"reasoning_effort":"xhigh"}),
    ] {
        ChatCompletionsProvider::api_key("fixture-key", "http://127.0.0.1:9")?
            .with_reasoning_fields(fields)?;
    }
    Ok(())
}

#[tokio::test]
async fn llm_parses_sample_and_malformed_is_refusal() -> Result<()> {
    let predicate = Question {
        name: "safe".into(),
        instructions: "Safe?".into(),
        kind: QuestionKind::Predicate,
    };
    let score = Question {
        name: "severity".into(),
        instructions: "Severity?".into(),
        kind: QuestionKind::Score {
            levels: vec![
                ScoreLevel {
                    label: "low".into(),
                    description: "Minor".into(),
                },
                ScoreLevel {
                    label: "high".into(),
                    description: "Major".into(),
                },
            ],
        },
    };
    let req = request(vec![predicate, choice_question(), score]);
    let provider = LlmDecisions::new(Arc::new(ScriptedProvider::new(
        r#"{"safe":true,"choice":"1","severity":"high"}"#,
    )));
    let response = provider
        .decide(req.clone(), CancellationToken::new())
        .await?;
    assert!(matches!(
        response.answers[0].kind,
        AnswerKind::Predicate { probability: 1.0 }
    ));
    assert!(
        matches!(&response.answers[1].kind,AnswerKind::Choice{choice,probabilities} if choice=="1" && probabilities==&vec![("1".into(),1.0),("none".into(),0.0)])
    );
    assert!(matches!(&response.answers[2].kind,AnswerKind::Score{score,..} if *score==1.0));
    for text in [
        "not JSON",
        r#"{"safe":true,"choice":"bad","severity":"bad"}"#,
    ] {
        let response = LlmDecisions::new(Arc::new(ScriptedProvider::new(text)))
            .decide(req.clone(), CancellationToken::new())
            .await?;
        assert!(matches!(response.answers[1].kind, AnswerKind::Refusal));
        assert!(matches!(response.answers[2].kind, AnswerKind::Refusal));
    }
    Ok(())
}

#[tokio::test]
async fn role_skips_missing_key_adapters_and_missing_llm_providers() -> Result<()> {
    let registry =
        ProviderRegistry::new("main", Arc::new(ScriptedProvider::new(r#"{"safe":true}"#)));
    let role = ModelRole {
        models: vec![
            "openrouter/typesafe/jev-1.13".into(),
            "openai/gpt-6-luna".into(),
            "llm/missing/model".into(),
            "llm/main/fixture".into(),
        ],
        repeats: Some(3),
        threshold: Some(0.9),
        reasoning_effort: Default::default(),
        max_output_tokens: None,
    };
    let response = role
        .decide(
            &registry,
            request(vec![Question {
                name: "safe".into(),
                instructions: "Safe?".into(),
                kind: QuestionKind::Predicate,
            }]),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(response.model, "fixture");
    assert!(matches!(
        response.answers[0].kind,
        AnswerKind::Predicate { probability: 1.0 }
    ));
    Ok(())
}

#[tokio::test]
async fn role_repeats_average_predicate_and_score_samples() -> Result<()> {
    use bubblepaw_runner::provider::ScriptedStep;
    let registry = ProviderRegistry::new(
        "main",
        Arc::new(ScriptedProvider::with_steps(
            vec![
                ScriptedStep::Text {
                    text: r#"{"safe":true,"severity":"high"}"#.into(),
                },
                ScriptedStep::Text {
                    text: r#"{"safe":false,"severity":"low"}"#.into(),
                },
                ScriptedStep::Text {
                    text: r#"{"safe":false,"severity":"low"}"#.into(),
                },
            ],
            "unused",
        )),
    );
    let role = ModelRole {
        models: vec!["llm/main/fixture".into()],
        repeats: Some(3),
        threshold: Some(0.9),
        reasoning_effort: Default::default(),
        max_output_tokens: None,
    };
    let response = role
        .decide(
            &registry,
            request(vec![
                Question {
                    name: "safe".into(),
                    instructions: "Safe?".into(),
                    kind: QuestionKind::Predicate,
                },
                Question {
                    name: "severity".into(),
                    instructions: "Severity?".into(),
                    kind: QuestionKind::Score {
                        levels: vec![
                            ScoreLevel {
                                label: "low".into(),
                                description: "Minor".into(),
                            },
                            ScoreLevel {
                                label: "high".into(),
                                description: "Major".into(),
                            },
                        ],
                    },
                },
            ]),
            CancellationToken::new(),
        )
        .await?;
    assert!(
        matches!(response.answers[0].kind,AnswerKind::Predicate{probability} if (probability-1.0/3.0).abs()<1e-12)
    );
    assert!(
        matches!(&response.answers[1].kind,AnswerKind::Score{score,probabilities} if (*score-1.0/3.0).abs()<1e-12 && probabilities==&vec![("low".into(),2.0/3.0),("high".into(),1.0/3.0)])
    );
    Ok(())
}

#[tokio::test]
async fn invalid_openai_answer_has_fatal_kind_and_keeps_metered_usage() -> Result<()> {
    let (base, server) = fake_server(
        json!({"model":"fixture","usage":{"input_tokens":17},"answers":[{"name":"safe","type":"predicate","probability":3.0}]}),
    );
    let error = OpenAiDecisions::new("fixture-key", base)?
        .decide(
            request(vec![Question {
                name: "safe".into(),
                instructions: "Safe?".into(),
                kind: QuestionKind::Predicate,
            }]),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    server.join().unwrap();
    assert_eq!(
        bubblepaw_core::ProviderError::of(&error).unwrap().kind,
        bubblepaw_core::ProviderErrorKind::Fatal
    );
    assert_eq!(
        bubblepaw_runner::provider::decision_error_usage(&error)
            .unwrap()
            .input_tokens,
        17
    );
    Ok(())
}

#[tokio::test]
async fn openai_fake_server_parses_predicate_choice_score_refusal_and_full_usage() -> Result<()> {
    let payload = json!({"model":"gpt-6-luna","usage":{"input_tokens":42,"output_tokens":9,"cost_usd":0.001,"input_tokens_details":{"cached_tokens":20}},"answers":[{"name":"safe","type":"predicate","probability":0.95},{"name":"choice","type":"choice","choice":"1","probabilities":[{"value":"1","probability":0.8},{"value":"none","probability":0.2}]},{"name":"severity","type":"score","score":0.75,"probabilities":[{"label":"low","value":0,"probability":0.25},{"label":"high","value":1,"probability":0.75}]},{"name":"refused","type":"refusal"}]});
    let (base, server) = fake_server(payload);
    let mut req = request(vec![
        Question {
            name: "safe".into(),
            instructions: "Safe?".into(),
            kind: QuestionKind::Predicate,
        },
        choice_question(),
        Question {
            name: "severity".into(),
            instructions: "Severity?".into(),
            kind: QuestionKind::Score {
                levels: vec![
                    ScoreLevel {
                        label: "low".into(),
                        description: "Minor".into(),
                    },
                    ScoreLevel {
                        label: "high".into(),
                        description: "Major".into(),
                    },
                ],
            },
        },
        Question {
            name: "refused".into(),
            instructions: "Refuse?".into(),
            kind: QuestionKind::Predicate,
        },
    ]);
    req.model = "gpt-6-luna".into();
    let response = OpenAiDecisions::new("fixture-key", base)?
        .decide(req, CancellationToken::new())
        .await?;
    let (path, body) = server.join().unwrap();
    assert_eq!(path, "POST /v1/decisions HTTP/1.1");
    assert_eq!(body["questions"].as_array().unwrap().len(), 4);
    assert!(matches!(
        response.answers[0].kind,
        AnswerKind::Predicate { probability: 0.95 }
    ));
    assert!(matches!(&response.answers[1].kind,AnswerKind::Choice{choice,..} if choice=="1"));
    assert!(matches!(&response.answers[2].kind,AnswerKind::Score{score,..} if *score==0.75));
    assert!(matches!(response.answers[3].kind, AnswerKind::Refusal));
    assert_eq!(response.usage.input_tokens, 42);
    assert_eq!(response.usage.output_tokens, 9);
    assert_eq!(response.usage.cache_read_tokens, 20);
    assert_eq!(response.usage.cost_usd, Some(0.001));
    Ok(())
}

/// DeepSeek reports prompt-cache hits and misses instead of cached_tokens; the
/// prompt total is hit plus miss.
fn deepseek_usage() -> Value {
    json!({"prompt_tokens":100,"completion_tokens":5,"prompt_cache_hit_tokens":70,"prompt_cache_miss_tokens":30,"cost":0.002})
}
fn deepseek_choice_answer() -> Value {
    json!({"model":"fixture","answers":{"choice":{"type":"choice","choice":"none","probabilities":{"1":0.1,"none":0.9}}},"usage":deepseek_usage()})
}
fn assert_deepseek_consumer_usage(usage: &DecisionUsage) {
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.cache_read_tokens, 70);
    assert_eq!(usage.cache_creation_tokens, 0);
}

#[tokio::test]
async fn openrouter_deepseek_usage_reaches_role_totals_and_ledger() -> Result<()> {
    let (base, server) = fake_server(deepseek_choice_answer());
    let mut registry = ProviderRegistry::new("main", Arc::new(ScriptedProvider::new("unused")));
    registry.insert_decision(
        "openrouter",
        Arc::new(OpenRouterDecisions::new(
            "fixture-key",
            format!("{base}/decisions"),
        )?),
    )?;
    let role = ModelRole {
        models: vec!["openrouter/fixture".into()],
        repeats: Some(1),
        threshold: None,
        reasoning_effort: Default::default(),
        max_output_tokens: None,
    };
    let (result, rows) = bubblepaw_core::capture_request_usage(
        "fixture-session",
        None,
        bubblepaw_core::RequestKind::Decider,
        async {
            role.decide(
                &registry,
                request(vec![choice_question()]),
                CancellationToken::new(),
            )
            .await
        },
    )
    .await;
    let result = result?;
    server.join().unwrap();
    assert_deepseek_consumer_usage(&result.usage);
    assert!((result.usage.cost_usd.unwrap() - 0.002).abs() < 1e-12);
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].failed);
    assert_eq!(rows[0].usage.input_tokens, 100);
    assert_eq!(rows[0].usage.output_tokens, 5);
    assert_eq!(rows[0].usage.cache_read_tokens, 70);
    Ok(())
}

#[tokio::test]
async fn opencode_deepseek_usage_reaches_decision_response() -> Result<()> {
    let (base, server) = fake_server(deepseek_choice_answer());
    let response = OpenCodeDecisions::new("fixture-key", format!("{base}/decisions"))?
        .decide(request(vec![choice_question()]), CancellationToken::new())
        .await?;
    server.join().unwrap();
    assert_deepseek_consumer_usage(&response.usage);
    assert_eq!(response.usage.cost_usd, Some(0.002));
    Ok(())
}

#[tokio::test]
async fn openai_deepseek_usage_reaches_decision_response() -> Result<()> {
    let (base, server) = fake_server(json!({
        "model":"fixture",
        "answers":[{"name":"choice","type":"choice","choice":"none","probabilities":[{"value":"1","probability":0.1},{"value":"none","probability":0.9}]}],
        "usage":deepseek_usage()
    }));
    let response = OpenAiDecisions::new("fixture-key", base)?
        .decide(request(vec![choice_question()]), CancellationToken::new())
        .await?;
    server.join().unwrap();
    assert_deepseek_consumer_usage(&response.usage);
    assert_eq!(response.usage.cost_usd, Some(0.002));
    Ok(())
}

#[tokio::test]
async fn llm_deepseek_chat_usage_reaches_decision_response() -> Result<()> {
    use bubblepaw_runner::provider::ChatCompletionsProvider;
    let (base, server) = fake_server(json!({
        "choices":[{"message":{"content":"{\"choice\":\"none\"}"}}],
        "usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_cache_hit_tokens":70,"prompt_cache_miss_tokens":30}
    }));
    let provider = ChatCompletionsProvider::api_key("fixture-key", base)?;
    let (response, _) = LlmDecisions::new(Arc::new(provider))
        .decide_with_reasoning(request(vec![choice_question()]), CancellationToken::new())
        .await?;
    server.join().unwrap();
    assert_deepseek_consumer_usage(&response.usage);
    Ok(())
}

/// Metered usage of a malformed answer, as the role and ledger see it.
fn assert_malformed_usage_kept(error: &anyhow::Error) {
    let decision = bubblepaw_runner::provider::decision_error_usage(error).unwrap();
    assert_eq!(
        (
            decision.input_tokens,
            decision.output_tokens,
            decision.cache_read_tokens,
            decision.cache_creation_tokens,
            decision.cost_usd,
        ),
        (50, 8, 20, 5, Some(0.004))
    );
    let provider_error = bubblepaw_core::ProviderError::of(error).unwrap();
    assert_eq!(
        provider_error.kind,
        bubblepaw_core::ProviderErrorKind::Fatal
    );
    let usage = provider_error.usage.as_ref().unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
        ),
        (50, 8, 20, 5)
    );
}

#[tokio::test]
async fn malformed_native_answers_keep_output_and_cache_in_errors() -> Result<()> {
    let usage = json!({"input_tokens":50,"output_tokens":8,"cost":0.004,
        "input_tokens_details":{"cached_tokens":20,"cache_creation_tokens":5}});
    let bad_probability = json!([{"name":"choice","type":"choice","choice":"none",
        "probabilities":[{"value":"1","probability":3.0},{"value":"none","probability":0.0}]}]);
    let bad_choice = json!({"model":"fixture","answers":{"choice":{"type":"choice",
        "choice":"bogus","probabilities":{"1":0.1,"none":0.9}}},"usage":usage});

    let (base, server) =
        fake_server(json!({"model":"fixture","usage":usage,"answers":bad_probability}));
    let error = OpenAiDecisions::new("fixture-key", base)?
        .decide(request(vec![choice_question()]), CancellationToken::new())
        .await
        .unwrap_err();
    server.join().unwrap();
    assert_malformed_usage_kept(&error);

    let (base, server) = fake_server(bad_choice.clone());
    let error = OpenRouterDecisions::new("fixture-key", format!("{base}/decisions"))?
        .decide(request(vec![choice_question()]), CancellationToken::new())
        .await
        .unwrap_err();
    server.join().unwrap();
    assert_malformed_usage_kept(&error);

    let (base, server) = fake_server(bad_choice);
    let error = OpenCodeDecisions::new("fixture-key", format!("{base}/decisions"))?
        .decide(request(vec![choice_question()]), CancellationToken::new())
        .await
        .unwrap_err();
    server.join().unwrap();
    assert_malformed_usage_kept(&error);
    Ok(())
}

#[tokio::test]
async fn explicit_nested_zero_cache_is_not_replaced_by_top_level_alias() -> Result<()> {
    let (base, server) = fake_server(
        json!({"model":"fixture","answers":{"choice":{"type":"choice",
        "choice":"none","probabilities":{"1":0.1,"none":0.9}}},
        "usage":{"input_tokens":50,"output_tokens":8,"cache_read_tokens":9,
            "cache_creation_tokens":4,"input_tokens_details":{"cached_tokens":0,"cache_creation_tokens":0}}}),
    );
    let response = OpenRouterDecisions::new("fixture-key", format!("{base}/decisions"))?
        .decide(request(vec![choice_question()]), CancellationToken::new())
        .await?;
    server.join().unwrap();
    assert_eq!(
        (
            response.usage.cache_read_tokens,
            response.usage.cache_creation_tokens
        ),
        (0, 0)
    );
    Ok(())
}

#[tokio::test]
async fn opencode_native_jev_fixture_routes_session_header_and_falls_back_with_usage() -> Result<()>
{
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use bubblepaw_runner::provider::OpenCodeDecisions;
    use std::sync::Mutex;
    type Seen = Arc<Mutex<Vec<(String, Value)>>>;
    async fn upstream(
        State((seen, free_status)): State<(Seen, StatusCode)>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        seen.lock().unwrap().push((
            headers["x-opencode-session"].to_str().unwrap().into(),
            body.clone(),
        ));
        assert!(
            body.get("session_id").is_none(),
            "OpenCode rejects this body field"
        );
        if body["model"] == "jev-1.13-free" && free_status != StatusCode::OK {
            return (
                free_status,
                Json(
                    json!({"error":{"type":"FreeTierError"},"usage":{"input_tokens":7,"output_tokens":1}}),
                ),
            );
        }
        let mut fixture: Value =
            serde_json::from_str(include_str!("fixtures/opencode-jev-20261018-working.json"))
                .unwrap();
        fixture["response"]["model"] = body["model"].clone();
        (StatusCode::OK, Json(fixture["response"].take()))
    }
    for free_status in [
        StatusCode::FORBIDDEN,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::OK,
    ] {
        let free_ok = free_status == StatusCode::OK;
        let failed_attempts = if free_ok {
            0
        } else if free_status == StatusCode::FORBIDDEN {
            1
        } else {
            4
        };
        let seen: Seen = Default::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}/systemone", listener.local_addr()?);
        let app = Router::new()
            .route("/systemone", post(upstream))
            .with_state((seen.clone(), free_status));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut registry = ProviderRegistry::new("main", Arc::new(ScriptedProvider::new("unused")));
        registry.insert_decision(
            "opencode-decisions",
            Arc::new(OpenCodeDecisions::new("synthetic", base)?),
        )?;
        let role = ModelRole {
            models: vec![
                "unavailable/jev".into(),
                "opencode-decisions/jev-1.13-free".into(),
                "opencode-decisions/jev-1.13".into(),
            ],
            repeats: Some(1),
            threshold: Some(0.0),
            reasoning_effort: Default::default(),
            max_output_tokens: None,
        };
        let mut req = request(vec![Question {
            name: "choice".into(),
            instructions: "Select the stated word.".into(),
            kind: QuestionKind::Choice {
                choices: vec![
                    DecisionChoice {
                        value: "apple".into(),
                        description: "The stated word is apple.".into(),
                    },
                    DecisionChoice {
                        value: "pear".into(),
                        description: "The stated word is pear.".into(),
                    },
                ],
            },
        }]);
        req.input = "Synthetic check: the word is apple.".into();
        let (response, _, selected) = role
            .decide_with_retry_selected(
                &registry,
                req.clone(),
                CancellationToken::new(),
                std::time::Duration::ZERO,
                Arc::new(|_, _, _| {}),
            )
            .await?;
        let expected = if free_ok { "jev-1.13-free" } else { "jev-1.13" };
        assert_eq!(selected, format!("opencode-decisions/{expected}"));
        assert_eq!(response.model, expected);
        assert_eq!(response.usage.input_tokens, 325 + failed_attempts * 7);
        assert_eq!(response.usage.output_tokens, 31 + failed_attempts);
        assert!(
            matches!(&response.answers[0].kind, AnswerKind::Choice{choice,probabilities} if choice=="apple" && probabilities==&vec![("apple".into(),1.0),("pear".into(),0.0)])
        );
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len() as u64, 1 + failed_attempts);
        assert!(requests.iter().all(|(session, _)| session == "s"));
        assert_eq!(
            requests[0].1,
            OpenCodeDecisions::body(&DecisionRequest {
                model: "jev-1.13-free".into(),
                ..req
            })
        );
        server.abort();
    }
    Ok(())
}
#[tokio::test]
async fn decision_cancelled_before_selection_does_not_fall_through_even_if_unavailable() {
    let registry = ProviderRegistry::new("main", Arc::new(ScriptedProvider::new("unused")));
    let role = ModelRole {
        models: vec!["absent/free".into(), "absent/paid".into()],
        repeats: Some(1),
        threshold: None,
        reasoning_effort: Default::default(),
        max_output_tokens: None,
    };
    let token = CancellationToken::new();
    token.cancel();
    let error = role
        .decide(&registry, request(vec![choice_question()]), token)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
}

#[tokio::test]
async fn physical_native_decision_usage_keeps_reported_cache_and_reasoning() -> Result<()> {
    let reported = json!({"input_tokens":50,"output_tokens":8,
        "input_tokens_details":{"cached_tokens":20,"cache_creation_tokens":5},
        "output_tokens_details":{"reasoning_tokens":3}});
    let (base, server) = fake_server_sequence(vec![
        json!({"model":"fixture","answers":{"choice":{"type":"choice","choice":"none","probabilities":{"1":0.1,"none":0.9}}},"usage":reported}),
        json!({"model":"fixture","answers":{},"usage":reported}),
    ]);
    let provider = OpenRouterDecisions::new("fixture-key", format!("{base}/decisions"))?;
    let (result, rows) = bubblepaw_core::capture_request_usage(
        "fixture-session",
        None,
        bubblepaw_core::RequestKind::Decider,
        async {
            provider
                .decide(request(vec![choice_question()]), CancellationToken::new())
                .await?;
            assert!(
                provider
                    .decide(request(vec![choice_question()]), CancellationToken::new())
                    .await
                    .is_err()
            );
            Ok::<_, anyhow::Error>(())
        },
    )
    .await;
    result?;
    server.join().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(!rows[0].failed);
    assert!(rows[1].failed);
    for row in rows {
        assert_eq!(row.session_id, "fixture-session");
        assert_eq!(row.kind, bubblepaw_core::RequestKind::Decider);
        assert_eq!(row.usage.input_tokens, 50);
        assert_eq!(row.usage.cache_read_tokens, 20);
        assert_eq!(row.usage.cache_creation_tokens, 5);
        assert_eq!(row.usage.output_tokens, 8);
        assert_eq!(row.usage.reasoning_tokens, Some(3));
    }
    Ok(())
}
