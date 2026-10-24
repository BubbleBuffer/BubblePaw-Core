use anyhow::Result;
use async_trait::async_trait;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use bubblepaw_core::{
    Answer, AnswerKind, CancellationToken, ContextSubjectChapter, DecisionProvider,
    DecisionRequest, DecisionResponse, DecisionUsage, MessageOrigin, ModelRequest, ModelResponse,
    Provider, ProviderError, ProviderErrorKind, ReasoningEffort, SubjectChapterStatus,
    TextCallback, Usage,
};
use bubblepaw_core::{Harness, HarnessConfig, RunOptions, Store, ToolRegistry};
use bubblepaw_runner::episode_models::*;
use bubblepaw_runner::trace::{JsonlTrace, TraceConfig};
use bubblepaw_runner::{
    episode_worker::EpisodeWorker,
    summarization::{SummarizingContext, SummaryConfig},
};
use bubblepaw_runner::{
    model_roles::ModelRoles,
    provider::{ProviderEndpoints, ProviderKeys, ProviderRegistry},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct NoProvider;
#[async_trait]
impl Provider for NoProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("Unexpected main provider call")
    }
}
type Replies = Arc<Mutex<VecDeque<(StatusCode, Value)>>>;
struct FakeModels {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    decisions: Arc<Mutex<Vec<DecisionRequest>>>,
    replies: Replies,
}
async fn serve(replies: Vec<(StatusCode, Value)>) -> Arc<FakeModels> {
    Arc::new(FakeModels {
        requests: Arc::default(),
        decisions: Arc::default(),
        replies: Arc::new(Mutex::new(replies.into())),
    })
}
impl FakeModels {
    fn reply(&self) -> Result<Value> {
        let (status, payload) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("Unplanned model call");
        if status.is_success() {
            return Ok(payload);
        }
        let error = anyhow::Error::new(
            ProviderError::new(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    ProviderErrorKind::RateLimit { resets_at: None }
                } else if status.is_server_error() {
                    ProviderErrorKind::Transient
                } else {
                    ProviderErrorKind::Fatal
                },
                "fixture failure",
            )
            .with_published(payload["published"].as_bool().unwrap_or(false)),
        );
        Err(if payload["context"].as_bool().unwrap_or(false) {
            error.context("synthetic provider context")
        } else {
            error
        })
    }
}
#[async_trait]
impl Provider for FakeModels {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let payload = self.reply()?;
        Ok(ModelResponse {
            text: payload["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .into(),
            usage: Some(Usage {
                input_tokens: 17,
                output_tokens: 4,
                ..Default::default()
            }),
            provider_state: Some(json!({"reasoning_content":"trace"})),
            ..Default::default()
        })
    }
}
#[async_trait]
impl DecisionProvider for FakeModels {
    async fn decide(
        &self,
        request: DecisionRequest,
        _: CancellationToken,
    ) -> Result<DecisionResponse> {
        self.decisions.lock().unwrap().push(request);
        let payload = self.reply()?;
        let distribution = payload["distribution"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    if i == 2 {
                        "none".into()
                    } else {
                        (i + 1).to_string()
                    },
                    p.as_f64().unwrap(),
                )
            })
            .collect();
        Ok(DecisionResponse {
            model: payload["model"].as_str().unwrap().into(),
            answers: vec![Answer {
                name: "choice".into(),
                kind: AnswerKind::Choice {
                    choice: "2".into(),
                    probabilities: distribution,
                },
            }],
            usage: DecisionUsage {
                input_tokens: 11,
                output_tokens: 2,
                ..Default::default()
            },
        })
    }
}
fn models(server: &Arc<FakeModels>, decision: bool) -> HttpEpisodeModels {
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry.insert("opencode", server.clone()).unwrap();
    let mut roles = ModelRoles::for_models(
        "opencode/deepseek-v4.1-flash".into(),
        "main/fallback".into(),
    );
    // These transport fixtures exercise one route; fallback has separate tests.
    roles.summary.models.truncate(1);
    roles.chapter.models.truncate(1);
    if decision {
        registry.insert_decision("fixture", server.clone()).unwrap();
        roles.decider.models = vec!["fixture/decision-model".into()];
    } else {
        // No decision adapter is registered for Luna, so the configured LLM routes serve.
        roles.decider.models = vec![
            "openai/gpt-6-luna".into(),
            "llm/opencode/deepseek-v4.1-flash".into(),
            "llm/main/fallback".into(),
        ];
    }
    HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )
    .unwrap()
}
fn chat(text: &str) -> (StatusCode, Value) {
    (
        StatusCode::OK,
        json!({"choices":[{"message":{"content":text,"reasoning_content":"trace"}}],"usage":{"prompt_tokens":17,"completion_tokens":4}}),
    )
}
fn decision(p1: f64, p2: f64, none: f64) -> (StatusCode, Value) {
    (
        StatusCode::OK,
        json!({"model":"decision-model","distribution":[p1,p2,none]}),
    )
}
fn luna_decision(not_none: f64) -> (StatusCode, Value) {
    (
        StatusCode::OK,
        json!({"model":"gpt-6-luna","distribution":[0.1,not_none - 0.1,1.0-not_none]}),
    )
}
fn luna_models(server: &Arc<FakeModels>, roles: Option<ModelRoles>) -> HttpEpisodeModels {
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry.insert_decision("openai", server.clone()).unwrap();
    let mut roles = roles.unwrap_or_default();
    if roles.decider.models.is_empty() {
        roles.decider.models = vec!["openai/gpt-6-luna".into()];
    }
    HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )
    .unwrap()
}
fn split() -> SplitInput {
    SplitInput {
        session_id: "session".into(),
        open_chapter: "# prior\nsummary".into(),
        options: vec![
            SplitOption {
                message_id: "u1".into(),
                user: "first request".into(),
                agent_before: format!("OMIT{}tail", "x".repeat(400)),
            },
            SplitOption {
                message_id: "u2".into(),
                user: "next task".into(),
                agent_before: "answer".into(),
            },
        ],
    }
}
fn summary() -> SummaryInput {
    SummaryInput {
        focus: None,
        session_id: "session".into(),
        previous_summary: Some("# Earlier\nPrior facts".into()),
        turn_request: Some("do original task".into()),
        episode_text: "agent work".into(),
        cap: 2000,
    }
}

struct LowSummaryProvider {
    requests: Mutex<Vec<ModelRequest>>,
    response: String,
}
#[async_trait]
impl Provider for LowSummaryProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("Classical must use the low reasoning override")
    }
    async fn generate_decision(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: self.response.clone(),
            usage: Some(Usage::default()),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn classical_uses_summary_role_low_reasoning_and_keeps_long_output_without_retry()
-> Result<()> {
    let output = format!("  {}\n", "summary detail ".repeat(3_000));
    let provider = Arc::new(LowSummaryProvider {
        requests: Mutex::default(),
        response: output.clone(),
    });
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry.insert("summary-fixture", provider.clone())?;
    let mut roles = ModelRoles::default();
    roles.summary.models = vec![
        "missing/skipped".into(),
        "summary-fixture/chosen-summary".into(),
    ];
    let adapter = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )?;
    let result = adapter
        .summarize_classical(ClassicalInput {
            session_id: "classical-session".into(),
            previous_summary: Some("anchored earlier details".into()),
            latest_user_request: Some("please keep this EXACT".into()),
            span_text: "only the new span".into(),
        })
        .await?;
    assert_eq!(result.summary, output);
    assert_eq!(result.model, "chosen-summary");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "chosen-summary");
    assert!(requests[0].tools.is_empty());
    assert!(requests[0].session_id.is_empty());
    assert!(
        requests[0].messages[0]
            .content
            .contains("anchored earlier details")
    );
    assert!(
        requests[0].messages[0]
            .content
            .contains("please keep this EXACT")
    );
    assert!(
        requests[0].messages[0]
            .content
            .contains("only the new span")
    );
    assert!(
        requests[0]
            .system_prompt
            .contains("Stale or superseded asks")
    );
    Ok(())
}

#[tokio::test]
async fn classical_output_is_kept_whole_without_a_size_backstop() -> Result<()> {
    let provider = Arc::new(LowSummaryProvider {
        requests: Mutex::default(),
        response: "x".repeat(64 * 1024 + 1),
    });
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry.insert("summary-fixture", provider.clone())?;
    let mut roles = ModelRoles::default();
    roles.summary.models = vec!["summary-fixture/chosen-summary".into()];
    let adapter = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )?;
    let result = adapter
        .summarize_classical(ClassicalInput {
            session_id: "classical-session".into(),
            previous_summary: None,
            latest_user_request: None,
            span_text: "new span".into(),
        })
        .await?;
    assert_eq!(result.summary.len(), 64 * 1024 + 1);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    Ok(())
}
fn chapter(id: &str) -> ContextSubjectChapter {
    ContextSubjectChapter {
        id: id.into(),
        session_id: "session".into(),
        ordinal: 0,
        first_episode_ordinal: 0,
        last_episode_ordinal: 0,
        status: SubjectChapterStatus::Closed,
        description: Some("Old work".into()),
        summary: Some("Full prior summary".into()),
        related_ids: Vec::new(),
        related_evidence: Vec::new(),
        model: None,
        usage: None,
        reasoning: None,
    }
}

#[tokio::test]
async fn selected_luna_uses_one_sample_and_inclusive_point_six_threshold() {
    for (not_none, choice) in [(0.59, None), (0.6, Some("u2"))] {
        let server = serve(vec![luna_decision(not_none)]).await;
        let result = luna_models(&server, None).decide(split()).await.unwrap();
        assert_eq!(result.choice.as_deref(), choice);
        assert_eq!(result.model, "gpt-6-luna");
        assert_eq!(result.usage.input_tokens, 11);
        let requests = server.decisions.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, "gpt-6-luna");
    }
}

#[tokio::test]
async fn split_prompt_keeps_verification_milestones_in_one_phase() {
    // Non-Luna decision routes use three samples unless explicitly overridden.
    let server = serve(vec![decision(0.1, 0.1, 0.8); 3]).await;
    models(&server, true).decide(split()).await.unwrap();
    let requests = server.decisions.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let question = &requests[0].questions[0].instructions;
    assert!(question.contains("investigation to implementation or implementation to validation"));
    assert!(question.contains("Ongoing builds, tests, Clippy runs and their intermediate results remain one validation phase"));
    assert!(question.contains("Changing approach or correcting a failed attempt within the same concrete goal does not automatically start a new chapter"));
}

#[tokio::test]
async fn missing_luna_key_uses_llm_fallback_with_three_samples_and_point_eight_threshold() {
    for (votes, choice) in [(2, None), (3, Some("u2"))] {
        let server = serve(
            (0..3)
                .map(|sample| {
                    chat(if sample < votes {
                        "{\"choice\":\"2\"}"
                    } else {
                        "{\"choice\":\"none\"}"
                    })
                })
                .collect(),
        )
        .await;
        // Luna remains the configured primary; an absent adapter represents its missing key.
        let result = models(&server, false).decide(split()).await.unwrap();
        assert_eq!(result.choice.as_deref(), choice);
        assert!((1.0 - result.probabilities[2] - votes as f64 / 3.0).abs() < 1e-12);
        assert_eq!(result.model, "deepseek-v4.1-flash");
        assert_eq!(result.usage.input_tokens, 51);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| request.model == "deepseek-v4.1-flash")
        );
    }
}

#[tokio::test]
async fn explicit_split_overrides_win_independently_of_selected_luna_defaults() {
    use bubblepaw_runner::model_roles::ModelRoles;
    for (repeats, threshold, samples, choice) in [
        (Some(2), None, 2, Some("u2")),
        (None, Some(0.7), 1, None),
        (Some(2), Some(0.7), 2, None),
    ] {
        let server = serve(vec![luna_decision(0.6); samples]).await;
        let mut roles = ModelRoles::for_models("main/main-model".into(), "main/main-model".into());
        roles.decider.repeats = repeats;
        roles.decider.threshold = threshold;
        let result = luna_models(&server, Some(roles))
            .decide(split())
            .await
            .unwrap();
        assert_eq!(result.choice.as_deref(), choice);
        assert_eq!(server.decisions.lock().unwrap().len(), samples);
        assert_eq!(result.usage.input_tokens, 11 * samples as u64);
    }
}

#[tokio::test]
async fn summaries_and_chapters_send_reasoning_context_and_record_trace() {
    let server = serve(vec![
        chat("# Episode\nFacts\nOpen: follow up"),
        chat("# Chapter\nBuilt it.\nRelated: A (user: \"original task\")"),
    ])
    .await;
    let models = models(&server, false);
    let summary = models.summarize(summary()).await.unwrap();
    assert_eq!(summary.reasoning.as_deref(), Some("trace"));
    assert_eq!(summary.title, "Episode");
    let labeled_episode = format!(
        "EPISODE 7 [source source-first..source-last]\n{}",
        summary.summary
    );
    let plain_episode =
        "# Plain replay memory\nComplete replayed evidence [plain-source] [plain-call].";
    let closed = models
        .close_chapter(ChapterInput {
            session_id: "session".into(),
            episodes: vec![labeled_episode.clone(), plain_episode.into()],
            previous_chapters: vec![chapter("A")],
            typed_user_texts: vec!["do original task".into()],
            task_instructions: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(closed.description, "Chapter");
    assert_eq!(closed.summary, "Built it.");
    assert_eq!(closed.related_ids, vec!["A"]);
    assert_eq!(closed.related_evidence[0].quote, "original task");
    assert_eq!(closed.related_evidence[0].related_quote_matched, Some(true));
    assert_eq!(closed.reasoning.as_deref(), Some("trace"));
    let requests = server.requests.lock().unwrap();
    assert!(requests.iter().all(|request| request.tools.is_empty()));
    let user = &requests[0].messages[0].content;
    assert!(user.contains("TASK ANCHOR (read-only, may be a child task):\ndo original task"));
    assert!(user.contains("PREVIOUS EPISODE (read-only):\n# Earlier\nPrior facts"));
    assert!(user.contains("CURRENT EPISODE:\nagent work"));
    // The transcript never ends the request: a trailing instruction does.
    assert!(user.ends_with(
        "agent work\n\nEND OF CURRENT EPISODE. Summarize the episode above. Do not continue it and do not call tools."
    ));
    let system = &requests[0].system_prompt;
    assert!(system.contains("about 2000 tokens (model tokens, not words), at most 2000 tokens"));
    let chapter_user = &requests[1].messages[0].content;
    assert!(chapter_user.contains("A:\n# Old work"));
    assert_eq!(chapter_user.matches(labeled_episode.as_str()).count(), 1);
    assert_eq!(chapter_user.matches(plain_episode).count(), 1);
    assert_eq!(chapter_user.matches("BEGIN EPISODE MEMORY\n").count(), 2);
    assert_eq!(chapter_user.matches("\nEND EPISODE MEMORY").count(), 2);
    assert!(chapter_user.contains(&format!(
        "BEGIN EPISODE MEMORY\n{labeled_episode}\nEND EPISODE MEMORY"
    )));
    assert!(chapter_user.contains(&format!(
        "BEGIN EPISODE MEMORY\n{plain_episode}\nEND EPISODE MEMORY"
    )));
    assert!(!chapter_user.contains("EPISODE 0:"));
    assert!(!chapter_user.contains("EPISODE 1:"));
    assert!(!chapter_user.contains("Full prior summary"));
    assert!(requests[1].system_prompt.contains("at most 8000 tokens"));
    assert!(requests[1].system_prompt.contains("at most 300 tokens"));
}
#[tokio::test]
async fn chapter_continuity_uses_actual_child_instructions_and_preserves_source_references() {
    let server = serve(vec![chat("# Continue implementation\nObserved failure source-m2/call-c2; validation still open.\nRelated: A (task: \"resume child implementation\")")]).await;
    let result = models(&server, false)
        .close_chapter(ChapterInput {
            session_id: "child-session".into(),
            episodes: vec![
                "# Implementation\nObserved exit 2 [source-m2] [call-c2]. Open: validation.".into(),
            ],
            previous_chapters: vec![chapter("A")],
            typed_user_texts: vec!["Human request has a different goal".into()],
            task_instructions: vec![
                "Please resume child implementation and preserve the failing check".into(),
            ],
        })
        .await
        .unwrap();
    assert_eq!(result.related_ids, vec!["A"]);
    assert_eq!(
        result.related_evidence[0].quote,
        "resume child implementation"
    );
    assert_eq!(
        result.related_evidence[0].origin,
        Some(MessageOrigin::ChildTask)
    );
    assert_eq!(result.related_evidence[0].related_quote_matched, Some(true));
    let requests = server.requests.lock().unwrap();
    let input = &requests[0].messages[0].content;
    assert!(input.contains("TYPED REQUEST:\nHuman request"));
    assert!(input.contains("DELEGATED CHILD TASK (not a human statement):\nPlease resume child"));
    assert!(input.contains("[source-m2] [call-c2]"));
    assert!(result.summary.contains("source-m2/call-c2"));
    assert!(result.summary.contains("validation still open"));
}

#[test]
fn related_quote_provenance_is_verified_against_corresponding_sources() {
    let previous: Vec<_> = ["A", "B", "C", "D", "E", "F"]
        .into_iter()
        .map(chapter)
        .collect();
    let (_, ids, evidence) = parse_related_with_tasks(
        "# State\nOpen work.\nRelated: A (user: \"human goal\"); B (task: \"child goal\"); C (request: \"child goal\"); D (user: \"child goal\"); E (task: \"human goal\"); F (request: \"shared goal\")",
        &previous,
        &["Resume human goal; shared goal".into()],
        &["Resume child goal; shared goal".into()],
    );
    assert_eq!(ids, ["A", "B", "C", "D", "E", "F"]);
    assert_eq!(
        evidence.iter().map(|item| item.origin).collect::<Vec<_>>(),
        [
            Some(MessageOrigin::Typed),
            Some(MessageOrigin::ChildTask),
            Some(MessageOrigin::ChildTask),
            None,
            None,
            Some(MessageOrigin::Typed)
        ]
    );
    assert_eq!(
        evidence
            .iter()
            .map(|item| item.related_quote_matched)
            .collect::<Vec<_>>(),
        [
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            Some(false),
            Some(true)
        ]
    );
    assert_eq!(evidence[1].quote, "child goal");
}

#[tokio::test]
async fn decision_adapter_bounds_custom_open_state_with_configured_token_budget() {
    use bubblepaw_runner::episodes::EpisodeConfig;
    let server = serve(vec![decision(0.0, 0.0, 1.0); 3]).await;
    let adapter = models(&server, true)
        .with_episode_config(EpisodeConfig {
            decision_context_tokens: 32,
            ..Default::default()
        })
        .unwrap();
    let mut input = split();
    input.open_chapter = "older unresolved state ".repeat(1000);
    assert!(adapter.decide(input).await.unwrap().choice.is_none());
    let calls = server.decisions.lock().unwrap();
    let text = &calls[0].input;
    let state = text
        .strip_prefix("ACCUMULATED OPEN CHAPTER STATE:\n")
        .unwrap()
        .split("\n\nNEXT BOUNDARIES")
        .next()
        .unwrap();
    let tokenizer =
        tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase).unwrap();
    assert!(tokenizer.encode_ordinary(state).len() <= 32);
}

#[test]
fn related_links_keep_unmatched_quotes_drop_unknown_and_parse_none() {
    let previous = vec![chapter("A"), chapter("B")];
    let (clean, ids, evidence) = parse_related(
        "# Title\nBody\nRelated: A (user: \"have; more work\"); B (user: \"not said\"); unknown (user: \"have more work\")",
        &previous,
        &["Have;  more work".into()],
    );
    assert_eq!(clean, "# Title\nBody");
    assert_eq!(ids, vec!["A", "B"]);
    assert_eq!(evidence[0].related_quote_matched, Some(true));
    assert_eq!(evidence[1].related_quote_matched, Some(false));
    assert!(
        parse_related("# Title\nBody\nRelated: none", &previous, &[])
            .1
            .is_empty()
    );
    assert!(
        parse_related(
            "# Title\nBody\nRelated: nope (user: \"hi\")",
            &previous,
            &[]
        )
        .1
        .is_empty()
    );
    assert!(
        parse_related("# Title\nBody\nRelated: A (user: \"broken)", &previous, &[])
            .1
            .is_empty()
    );
}
#[tokio::test]
async fn transport_retries_remain_bounded_without_cosmetic_summary_retries() {
    let server = serve(vec![
        (StatusCode::TOO_MANY_REQUESTS, json!({})),
        (StatusCode::BAD_GATEWAY, json!({})),
        chat("not titled"),
    ])
    .await;
    let result = models(&server, false).summarize(summary()).await.unwrap();
    assert_eq!(result.title, "not titled");
    assert_eq!(result.summary, "not titled");
    assert_eq!(result.usage.input_tokens, 17);
    assert_eq!(server.requests.lock().unwrap().len(), 3);
    let server = serve(vec![(StatusCode::SERVICE_UNAVAILABLE, json!({})); 4]).await;
    assert!(models(&server, false).summarize(summary()).await.is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 4);
    let server = serve(vec![chat(" \n\t "); 2]).await;
    assert!(models(&server, false).summarize(summary()).await.is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    let server = serve(vec![(StatusCode::BAD_REQUEST, json!({}))]).await;
    assert!(models(&server, false).summarize(summary()).await.is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn concise_summary_output_is_preserved_without_a_cosmetic_format_retry() {
    for (text, title) in [
        (
            "Observed exit 2 [source-m2/call-c2].\nOpen: verify persistence.",
            "Observed exit 2 [source-m2/call-c2].",
        ),
        (
            "One line of useful evidence.",
            "One line of useful evidence.",
        ),
        (
            "  # Valid heading\nBody with exact IDs.\n  ",
            "Valid heading",
        ),
    ] {
        let server = serve(vec![chat(text)]).await;
        let result = models(&server, false).summarize(summary()).await.unwrap();
        assert_eq!(result.title, title);
        assert_eq!(result.summary, text);
        assert_eq!(result.usage.input_tokens, 17);
        assert_eq!(result.reasoning.as_deref(), Some("trace"));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    let text = "測".repeat(500);
    let server = serve(vec![chat(&text)]).await;
    let result = models(&server, false).summarize(summary()).await.unwrap();
    assert_eq!(result.title.chars().count(), 120);
    assert_eq!(result.summary, text);
}

#[tokio::test]
async fn recorded_protocol_replies_and_attempted_actions_are_not_episode_memory() {
    for text in [
        "\n\n<｜DSML｜ calls>\n<｜DSML｜ invoke name=\"js\">\n<｜DSML｜ parameter name=\"code\" string=\"true\">const a = await tools.read({path: \"crates/bubblepaw-core/src/provider_ext.rs\", startLine: 1, lineCount: 200});\nreturn a.content;</｜DSML｜ parameter>\n</｜DSML｜ invoke>\n</｜DSML｜ calls>",
        "\n\n<｜DSML｜ calls>\n<｜DSML｜ invoke name=\"poll\" id=\"call_00_poll1\">\n{\"handle\":\"process:pending\",\"yieldMs\":30000}\n</｜DSML｜ parameter>\n</｜DSML｜ invoke>\n</｜DSML｜ calls>",
        "```javascript\nconst r = await tools.poll({handle:'process:pending'});\nreturn r;\n```",
        "{\"name\":\"read\",\"arguments\":{\"path\":\"source.rs\"}}",
        "# \nThe title is empty.",
        "# Heading only",
    ] {
        let server = serve(vec![chat(text)]).await;
        let error = models(&server, false)
            .summarize(summary())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid episode memory"));
        let usage = ProviderError::of(&error).unwrap().usage.as_ref().unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (17, 4));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    let text =
        "The provider emitted `<｜DSML｜ calls>` as text; no tool ran. Open: inspect the response.";
    let server = serve(vec![chat(text)]).await;
    assert_eq!(
        models(&server, false)
            .summarize(summary())
            .await
            .unwrap()
            .summary,
        text
    );
}

/// Shapes Flash produced in the 8k py-textkit qualification run (2026-10-19),
/// plus the other protocol and renderer forms, each after a prose lead-in.
#[tokio::test]
async fn embedded_markup_after_prose_is_rejected_and_traced_with_its_reason() {
    let lead = "Step 2 verified (11 tests OK). Now step 3.\n\n";
    for (shape, reason) in [
        (
            "<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"js\">",
            "tool_protocol_output",
        ),
        (
            "<｜DSML｜ calls>read</｜DSML｜ calls>",
            "tool_protocol_output",
        ),
        ("<||DSML|| invoke name=\"js\">", "tool_protocol_output"),
        ("<dsml:calls>read</dsml:calls>", "tool_protocol_output"),
        ("<invoke name=\"read\">\n</invoke>", "tool_protocol_output"),
        (
            "<tool_call>{\"name\":\"read\"}</tool_call>",
            "tool_protocol_output",
        ),
        (
            "<function_calls>\n<invoke name=\"read\"/>",
            "tool_protocol_output",
        ),
        (
            "*** Begin Patch\n*** Update File: a.py\n*** End Patch",
            "tool_protocol_output",
        ),
        (
            "Tool call call_00_Zq3n (js) arguments:\n{\"code\":\"1\"}",
            "transcript_output",
        ),
        (
            "Tool call call-id (exec) arguments (12 tokens):\n{}",
            "transcript_output",
        ),
        ("Tool call ID: call-id", "transcript_output"),
        (
            "[tool output call-id, 40 chars, ok]\nresult",
            "transcript_output",
        ),
        (
            "[9553d768-b8f4-45cb-998d-d837aeebd392 Assistant]\nNext.",
            "transcript_output",
        ),
        (
            "[source c24c50c0: Typed request]\nImplement it.",
            "transcript_output",
        ),
    ] {
        let text = format!("{lead}{shape}");
        let server = serve(vec![chat(&text)]).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.jsonl");
        let trace = JsonlTrace::open(TraceConfig {
            path: path.clone(),
            ..Default::default()
        })
        .unwrap();
        let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
        registry.insert("opencode", server.clone()).unwrap();
        let models = HttpEpisodeModels::from_registry(
            EpisodeModelConfig::default(),
            Arc::new(registry),
            Some(trace.clone()),
        )
        .unwrap();
        let error = models.summarize(summary()).await.unwrap_err();
        assert!(
            error.to_string().contains("invalid episode memory"),
            "{shape}: {error}"
        );
        trace.flush(Duration::from_secs(2)).unwrap();
        let failed: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["record"].clone())
            .filter(|record| record["metrics"]["event"] == "episode_model_output_failed")
            .collect();
        assert_eq!(failed.len(), 1, "{shape}");
        assert_eq!(failed[0]["metrics"]["reason"], reason, "{shape}");
    }
}

#[tokio::test]
async fn markup_quoted_in_a_fence_or_code_span_remains_valid_evidence() {
    for text in [
        "# Provider fault\nThe reply was protocol text, not a call:\n\n```\n<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"js\">\n```\n\nNo tool ran. Open: retry.",
        "Patched `count.py` with `*** Begin Patch` and saw `Tool call ID: x` echoed.\n\n~~~text\n[tool output call-id, 40 chars, error]\n~~~\nOpen: none.",
    ] {
        let server = serve(vec![chat(text)]).await;
        let result = models(&server, false).summarize(summary()).await.unwrap();
        assert_eq!(result.summary, text);
    }
    // An unclosed fence is not a quote.
    let server = serve(vec![chat("Next:\n```\n<｜｜DSML｜｜ calls>")]).await;
    assert!(models(&server, false).summarize(summary()).await.is_err());
}

#[tokio::test]
async fn invalid_memory_uses_role_retry_and_fallback_and_keeps_every_attempt_usage() {
    let bad = serve(vec![chat("<｜DSML｜ calls>invalid</｜DSML｜ calls>")]).await;
    let good = serve(vec![chat("Done. Open: none")]).await;
    let mut registry = ProviderRegistry::new("first", bad.clone());
    registry.insert("second", good.clone()).unwrap();
    let adapter = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: ModelRoles::for_models("first/flash".into(), "second/flash".into()),
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )
    .unwrap();
    let result = adapter.summarize(summary()).await.unwrap();
    assert_eq!(result.summary, "Done. Open: none");
    assert_eq!(
        (result.usage.input_tokens, result.usage.output_tokens),
        (34, 8)
    );
    assert_eq!(bad.requests.lock().unwrap().len(), 1);
    assert_eq!(good.requests.lock().unwrap().len(), 1);

    let provider = serve(vec![
        chat("<tool_call>read</tool_call>"),
        chat("Evidence retained."),
    ])
    .await;
    let mut roles = ModelRoles::for_models("single/flash".into(), String::new());
    roles.summary.repeats = Some(2);
    let adapter = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(ProviderRegistry::new("single", provider.clone())),
        None,
    )
    .unwrap();
    let result = adapter.summarize(summary()).await.unwrap();
    assert_eq!(result.summary, "Evidence retained.");
    assert_eq!(result.usage.input_tokens, 34);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_chapter_body_falls_back_before_accepting_memory() {
    let bad = serve(vec![chat("# Missing body\nRelated: none")]).await;
    let good = serve(vec![chat(
        "# Fix state\nVerified failure remains [source-id].\nRelated: none",
    )])
    .await;
    let mut registry = ProviderRegistry::new("first", bad.clone());
    registry.insert("second", good.clone()).unwrap();
    let adapter = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: ModelRoles::for_models("first/flash".into(), "second/flash".into()),
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )
    .unwrap();
    let result = adapter
        .close_chapter(ChapterInput {
            session_id: "session".into(),
            episodes: vec!["Evidence".into()],
            previous_chapters: vec![],
            typed_user_texts: vec![],
            task_instructions: vec![],
        })
        .await
        .unwrap();
    assert_eq!(result.description, "Fix state");
    assert_eq!(result.summary, "Verified failure remains [source-id].");
    assert_eq!(result.usage.input_tokens, 34);
}

#[tokio::test]
async fn untitled_chapter_keeps_prose_and_validates_related_evidence() {
    let text = "Observed exit 2 [source-m2/call-c2].\nOpen: verification.\nRelated: A (user: \"original task\")";
    let server = serve(vec![chat(text)]).await;
    let result = models(&server, false)
        .close_chapter(ChapterInput {
            session_id: "session".into(),
            episodes: vec!["Exact evidence".into()],
            previous_chapters: vec![chapter("A")],
            typed_user_texts: vec!["do original task".into()],
            task_instructions: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(result.description, "Observed exit 2 [source-m2/call-c2].");
    assert_eq!(
        result.summary,
        "Observed exit 2 [source-m2/call-c2].\nOpen: verification."
    );
    assert_eq!(result.related_ids, vec!["A"]);
    assert_eq!(result.related_evidence[0].quote, "original task");
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn chapter_without_content_after_related_extraction_fails_with_usage() {
    let server = serve(vec![chat(" \nRelated: none")]).await;
    let error = models(&server, false)
        .close_chapter(ChapterInput {
            session_id: "session".into(),
            episodes: vec!["Exact evidence".into()],
            previous_chapters: Vec::new(),
            typed_user_texts: Vec::new(),
            task_instructions: Vec::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        bubblepaw_runner::provider::decision_error_usage(&error)
            .unwrap()
            .input_tokens,
        17
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn chat_host_retries_only_unpublished_provider_errors_through_context() {
    for status in [StatusCode::BAD_GATEWAY, StatusCode::TOO_MANY_REQUESTS] {
        for published in [false, true] {
            let server = serve(vec![
                (status, json!({"published":published,"context":true})),
                chat("# Recovered\nBody"),
            ])
            .await;
            let result = models(&server, false).summarize(summary()).await;
            if published {
                let error = result.unwrap_err();
                let typed = ProviderError::of(&error).unwrap();
                assert!(typed.published);
                assert_eq!(
                    typed.kind,
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        ProviderErrorKind::RateLimit { resets_at: None }
                    } else {
                        ProviderErrorKind::Transient
                    }
                );
                assert_eq!(server.requests.lock().unwrap().len(), 1);
                assert_eq!(server.replies.lock().unwrap().len(), 1);
            } else {
                assert_eq!(result.unwrap().title, "Recovered");
                assert_eq!(server.requests.lock().unwrap().len(), 2);
                assert!(server.replies.lock().unwrap().is_empty());
            }
        }
    }
}

#[tokio::test]
async fn decision_role_retries_only_unpublished_provider_errors_through_context() {
    for status in [StatusCode::BAD_GATEWAY, StatusCode::TOO_MANY_REQUESTS] {
        for published in [false, true] {
            let server = serve(vec![
                (status, json!({"published":published,"context":true})),
                decision(0.1, 0.8, 0.1),
                decision(0.1, 0.8, 0.1),
                decision(0.1, 0.8, 0.1),
            ])
            .await;
            let result = models(&server, true).decide(split()).await;
            if published {
                let error = result.unwrap_err();
                let typed = ProviderError::of(&error).unwrap();
                assert!(typed.published);
                assert_eq!(
                    typed.kind,
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        ProviderErrorKind::RateLimit { resets_at: None }
                    } else {
                        ProviderErrorKind::Transient
                    }
                );
                assert_eq!(server.decisions.lock().unwrap().len(), 1);
                assert_eq!(server.replies.lock().unwrap().len(), 3);
            } else {
                assert_eq!(result.unwrap().choice.as_deref(), Some("u2"));
                assert_eq!(server.decisions.lock().unwrap().len(), 4);
                assert!(server.replies.lock().unwrap().is_empty());
            }
            assert!(server.requests.lock().unwrap().is_empty());
        }
    }
}
#[tokio::test]
async fn length_is_guidance_without_a_size_backstop() {
    let server = serve(vec![chat(&format!("# Long\n{}", "a".repeat(20000)))]).await;
    let mut input = summary();
    input.cap = 1;
    assert!(models(&server, false).summarize(input).await.is_ok());
    let server = serve(vec![chat(&format!("# Too long\n{}", "a".repeat(65536)))]).await;
    assert!(models(&server, false).summarize(summary()).await.is_ok());
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
struct MainFake {
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for MainFake {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let decide = request.system_prompt.contains("Allowed options:");
        self.requests.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: if decide {
                "{\"choice\":\"2\"}"
            } else {
                "# Main\nMain provider facts"
            }
            .into(),
            provider_state: Some(
                json!({"adapter":"responses","items":[{"type":"reasoning","summary":[{"text":"main trace"}]}]}),
            ),
            ..Default::default()
        })
    }
}
struct MainLarge;
#[async_trait]
impl Provider for MainLarge {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: " datum".repeat(17_000),
            ..Default::default()
        })
    }
}
struct FakeHttp {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve_http(replies: Vec<(StatusCode, Value)>) -> FakeHttp {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = (
        Arc::new(Mutex::new(VecDeque::from(replies))),
        requests.clone(),
    );
    async fn handler(
        State((replies, requests)): State<(Replies, Arc<Mutex<Vec<Value>>>)>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        requests.lock().unwrap().push(body);
        let (status, payload) = replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("Unplanned HTTP call");
        (status, Json(payload))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/chat/completions", post(handler))
                .with_state(state),
        )
        .await
        .unwrap();
    });
    FakeHttp {
        url,
        requests,
        task,
    }
}
fn http_models(server: &FakeHttp) -> HttpEpisodeModels {
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    ProviderEndpoints {
        opencode_base_url: Some(server.url.clone()),
        ..Default::default()
    }
    .register_with_keys(
        &mut registry,
        ProviderKeys {
            opencode: Some("fake-key".into()),
            ..Default::default()
        },
    )
    .unwrap();
    HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            retry_base_delay: Duration::ZERO,
            ..Default::default()
        },
        Arc::new(registry),
        None,
    )
    .unwrap()
}
#[tokio::test]
async fn actual_http_retry_exhaustion_retains_usage_without_derived_memory_and_main_harness_succeeds()
-> Result<()> {
    let replies = vec![(StatusCode::SERVICE_UNAVAILABLE, json!({})); 4];
    let server = serve_http(replies).await;
    let store = Store::in_memory()?;
    let session = store.create_session("HTTP exhausted retries")?.id;
    let provider = Arc::new(MainLarge);
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    for i in 0..3 {
        harness
            .prompt(&session, format!("request {i}"), RunOptions::default())
            .await?;
    }
    let estimator = Arc::new(SummarizingContext::new(
        store.clone(),
        provider,
        SummaryConfig::default(),
        None,
    )?);
    let worker = EpisodeWorker::new_with_config(
        store.clone(),
        Arc::new(http_models(&server)),
        estimator,
        bubblepaw_runner::episodes::EpisodeConfig {
            summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
            ..Default::default()
        },
    )?;
    let transactions = store.storage_stats()?.transaction_count;
    assert!(worker.trigger(&session, 50, 100));
    tokio::time::timeout(Duration::from_secs(15), worker.wait_idle(&session)).await?;
    assert_eq!(server.requests.lock().unwrap().len(), 4);
    assert!(store.context_episodes(&session)?.is_empty());
    assert!(store.context_subject_chapters(&session)?.is_empty());
    assert!(store.context_decisions(&session)?.is_empty());
    assert_eq!(store.storage_stats()?.transaction_count, transactions + 1);
    let auxiliary: Vec<_> = store
        .request_usage()?
        .into_iter()
        .filter(|row| row.kind == bubblepaw_core::RequestKind::SummaryFork)
        .collect();
    assert_eq!(auxiliary.len(), 4);
    assert!(auxiliary.iter().all(|row| row.failed));
    harness
        .prompt(&session, "main remains usable", RunOptions::default())
        .await?;
    assert_eq!(store.messages(&session)?.len(), 8);
    Ok(())
}
#[tokio::test]
async fn summary_and_chapter_roles_resolve_independently_and_skip_unavailable_keys() {
    use bubblepaw_runner::{model_roles::ModelRoles, provider::ProviderRegistry};
    let summary_provider = Arc::new(MainFake {
        requests: Mutex::new(Vec::new()),
    });
    let chapter_provider = Arc::new(MainFake {
        requests: Mutex::new(Vec::new()),
    });
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry
        .insert("summary", summary_provider.clone())
        .unwrap();
    registry
        .insert("chapter", chapter_provider.clone())
        .unwrap();
    let mut roles =
        ModelRoles::for_models("summary/summary-model".into(), "main/main-model".into());
    roles.summary.models.insert(0, "missing/no-key".into());
    roles.summary.reasoning_effort = ReasoningEffort::High;
    roles.chapter.models = vec!["missing/no-key".into(), "chapter/chapter-model".into()];
    roles.chapter.reasoning_effort = ReasoningEffort::Medium;
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            ..Default::default()
        },
        Arc::new(registry),
        None,
    )
    .unwrap();
    assert_eq!(
        models.summarize(summary()).await.unwrap().model,
        "summary-model"
    );
    assert_eq!(
        models
            .close_chapter(ChapterInput {
                session_id: "session".into(),
                episodes: vec!["# Episode\nFacts".into()],
                previous_chapters: vec![],
                typed_user_texts: vec![],
                task_instructions: Vec::new(),
            })
            .await
            .unwrap()
            .model,
        "chapter-model"
    );
    assert_eq!(summary_provider.requests.lock().unwrap().len(), 1);
    assert_eq!(chapter_provider.requests.lock().unwrap().len(), 1);
    assert_eq!(
        summary_provider.requests.lock().unwrap()[0].reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert_eq!(
        chapter_provider.requests.lock().unwrap()[0].reasoning_effort,
        Some(ReasoningEffort::Medium)
    );
}
#[tokio::test]
async fn configured_split_role_threshold_is_applied_without_changing_default() {
    use bubblepaw_runner::model_roles::ModelRoles;
    let server = serve(vec![decision(0.1, 0.75, 0.15); 3]).await;
    let mut roles =
        ModelRoles::for_models("opencode/deepseek-v4.1-flash".into(), "main/main".into());
    assert_eq!(roles.decider.threshold, None);
    assert_eq!(
        roles.decider.decision_settings("openai/gpt-6-luna"),
        (1, 0.6)
    );
    roles.decider.models = vec!["fixture/decision-model".into()];
    assert_eq!(
        roles.decider.decision_settings(&roles.decider.models[0]),
        (3, 0.8)
    );
    roles.decider.threshold = Some(0.9);
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry.insert_decision("fixture", server.clone()).unwrap();
    roles.decider.models = vec!["fixture/decision-model".into()];
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            ..Default::default()
        },
        Arc::new(registry),
        None,
    )
    .unwrap();
    let result = models.decide(split()).await.unwrap();
    assert_eq!(result.choice, None);
    assert!((1.0 - result.probabilities[2] - 0.85).abs() < 1e-12);
}
#[tokio::test]
async fn zero_split_threshold_honors_none_instead_of_forcing_a_chapter() {
    for (distribution, expected) in [
        ((0.0, 0.0, 1.0), None),
        ((0.1, 0.2, 0.7), None),
        ((0.5, 0.0, 0.5), None),
        ((0.2, 0.6, 0.2), Some("u2")),
    ] {
        let server = serve(vec![decision(
            distribution.0,
            distribution.1,
            distribution.2,
        )])
        .await;
        let mut roles =
            ModelRoles::for_models("opencode/deepseek-v4.1-flash".into(), "main/main".into());
        roles.decider.models = vec!["fixture/decision-model".into()];
        roles.decider.repeats = Some(1);
        roles.decider.threshold = Some(0.0);
        let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
        registry.insert_decision("fixture", server).unwrap();
        let models = HttpEpisodeModels::from_registry(
            EpisodeModelConfig {
                roles,
                ..Default::default()
            },
            Arc::new(registry),
            None,
        )
        .unwrap();
        assert_eq!(
            models.decide(split()).await.unwrap().choice.as_deref(),
            expected
        );
    }
    // The actual Flash profile uses an LLM choice with one episode boundary.
    for (answer, expected) in [
        ("{\"choice\":\"none\"}", None),
        ("{\"choice\":\"1\"}", Some("u1")),
        ("invalid JSON", None),
    ] {
        let server = serve(vec![chat(answer)]).await;
        let mut roles =
            ModelRoles::for_models("opencode/deepseek-v4.1-flash".into(), "main/main".into());
        roles.decider.models = vec!["llm/opencode/deepseek-v4.1-flash".into()];
        roles.decider.repeats = Some(1);
        roles.decider.threshold = Some(0.0);
        let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
        registry.insert("opencode", server).unwrap();
        let models = HttpEpisodeModels::from_registry(
            EpisodeModelConfig {
                roles,
                ..Default::default()
            },
            Arc::new(registry),
            None,
        )
        .unwrap();
        let mut input = split();
        input.options.truncate(1);
        assert_eq!(
            models.decide(input).await.unwrap().choice.as_deref(),
            expected
        );
    }
}
#[tokio::test]
async fn empty_summary_and_background_drain_failures_are_traced_with_usage() -> Result<()> {
    let server = serve(vec![chat(" \n\t "); 2]).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry.insert("opencode", server.clone()).unwrap();
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig::default(),
        Arc::new(registry),
        Some(trace.clone()),
    )
    .unwrap();
    let store = Store::in_memory()?;
    let session = store.create_session("empty summary diagnostics")?.id;
    let provider = Arc::new(MainLarge);
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    for i in 0..3 {
        harness
            .prompt(&session, format!("request {i}"), RunOptions::default())
            .await?;
    }
    let estimator = Arc::new(SummarizingContext::new(
        store.clone(),
        provider,
        SummaryConfig::default(),
        None,
    )?);
    let worker = EpisodeWorker::new_with_config(
        store.clone(),
        Arc::new(models),
        estimator,
        bubblepaw_runner::episodes::EpisodeConfig {
            summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
            ..Default::default()
        },
    )?;
    let transactions = store.storage_stats()?.transaction_count;
    assert!(worker.trigger(&session, 50, 100));
    tokio::time::timeout(Duration::from_secs(5), worker.wait_idle(&session)).await?;
    assert!(store.context_episodes(&session)?.is_empty());
    assert_eq!(store.storage_stats()?.transaction_count, transactions + 1);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    trace.flush(Duration::from_secs(2)).unwrap();
    assert_eq!(trace.status().dropped, 0);
    assert_eq!(trace.status().write_errors, 0);
    let records: Vec<Value> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            let envelope: Value = serde_json::from_str(line).unwrap();
            assert_eq!(envelope["schema_version"], 1);
            envelope["record"].clone()
        })
        .collect();
    let call = records
        .iter()
        .find(|value| value.to_string().contains("episode_model_call"))
        .unwrap()
        .to_string();
    assert!(call.contains("\"input_tokens\":17"));
    assert!(call.contains("\"output_tokens\":4"));
    assert!(call.contains("\"outcome\":\"error\""));
    let failed: Vec<_> = records
        .iter()
        .filter(|record| record["metrics"]["event"] == "episode_model_output_failed")
        .collect();
    assert_eq!(failed.len(), 2);
    assert_eq!(failed[0]["metrics"]["reason"], "empty_output");
    assert_eq!(failed[0]["metrics"]["usage"]["input_tokens"], 17);
    assert_eq!(failed[0]["metrics"]["usage"]["output_tokens"], 4);
    let drains: Vec<_> = records
        .iter()
        .filter(|record| record["metrics"]["event"] == "episode_drain_failed")
        .collect();
    assert_eq!(drains.len(), 1);
    assert_eq!(drains[0]["metrics"]["session_id"], session);
    assert_eq!(drains[0]["metrics"]["usage"]["input_tokens"], 34);
    assert_eq!(drains[0]["metrics"]["usage"]["output_tokens"], 8);
    assert!(records.iter().all(|record| record["payload"].is_null()));
    Ok(())
}
#[tokio::test]
async fn manual_focus_is_an_extra_summary_instruction_for_one_request() -> Result<()> {
    let provider = serve(vec![
        chat("# Episode\nExact decisions."),
        chat("# Episode\nExact decisions."),
    ])
    .await;
    let adapter = models(&provider, false);
    let mut focused = summary();
    focused.focus = Some("Preserve exact file paths and failed attempts".into());
    adapter.summarize(focused).await?;
    adapter.summarize(summary()).await?;
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .system_prompt
            .contains("Preserve exact file paths and failed attempts")
    );
    assert!(
        !requests[1]
            .system_prompt
            .contains("For this manual compaction")
    );
    assert!(requests.iter().all(|request| request.tools.is_empty()));
    Ok(())
}
#[tokio::test]
async fn fallback_decision_uses_successful_luna_calibration_instead_of_first_route() {
    let failed = serve(vec![(StatusCode::FORBIDDEN, json!({}))]).await;
    let luna = serve(vec![luna_decision(0.7)]).await;
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry.insert_decision("fixture", failed.clone()).unwrap();
    registry.insert_decision("openai", luna.clone()).unwrap();
    let mut roles = ModelRoles::default();
    roles.decider.models = vec!["fixture/jev-1.13-free".into(), "openai/gpt-6-luna".into()];
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )
    .unwrap();
    let result = models.decide(split()).await.unwrap();
    assert_eq!(result.model, "gpt-6-luna");
    // 0.7 qualifies for Luna's 0.6 threshold, but not the first route's 0.8.
    assert_eq!(result.choice.as_deref(), Some("u2"));
    assert_eq!(failed.decisions.lock().unwrap().len(), 1);
    assert_eq!(luna.decisions.lock().unwrap().len(), 1);
}

#[path = "episode_models/decider_config.rs"]
mod decider_config;
