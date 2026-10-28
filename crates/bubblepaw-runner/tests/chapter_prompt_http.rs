//! Local HTTP acceptance: prompt selection, parsing and the chapter commit share one path.
use anyhow::Result;
use async_trait::async_trait;
use axum::{Json, Router, extract::State, routing::post};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, ModelResponse, Provider, Store, SubjectChapterStatus,
    TextCallback, Usage,
};
use bubblepaw_runner::{
    episode_models::{EpisodeModelConfig, HttpEpisodeModels},
    episode_worker::EpisodeWorker,
    episodes::{ChapterPrompt, EpisodeConfig, EpisodeSummaryInput},
    model_roles::ModelRoles,
    provider::{ProviderEndpoints, ProviderKeys, ProviderRegistry},
    summarization::{SummarizingContext, SummaryConfig},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
#[path = "chapter_prompt_http/logs.rs"]
mod logs;

struct NoMainCalls;
#[async_trait]
impl Provider for NoMainCalls {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("unexpected main model request")
    }
}

type Replies = Arc<Mutex<VecDeque<String>>>;
type Requests = Arc<Mutex<Vec<Value>>>;
struct ScriptedTransport {
    replies: Replies,
    requests: Requests,
}
#[async_trait]
impl Provider for ScriptedTransport {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_value(request)?);
        let text = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unplanned scripted request");
        Ok(ModelResponse {
            text,
            usage: Some(Usage {
                input_tokens: 19,
                output_tokens: 7,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
struct Server {
    url: String,
    requests: Requests,
    replies: Replies,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
async fn serve(chapter: &str, http: bool) -> Result<Server> {
    async fn handler(
        State((replies, requests)): State<(Replies, Requests)>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        requests.lock().unwrap().push(body);
        let text = replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unplanned HTTP request");
        Json(json!({
            "choices":[{"message":{"content":text}}],
            "usage":{"prompt_tokens":19,"completion_tokens":7}
        }))
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let replies = Arc::new(Mutex::new(VecDeque::from([
        "# Parser episode\nParser fix passed at source msg-1.".to_owned(),
        chapter.to_owned(),
    ])));
    if !http {
        return Ok(Server {
            url: String::new(),
            requests,
            replies,
            task: None,
        });
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state((replies.clone(), requests.clone()));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok(Server {
        url,
        requests,
        replies,
        task: Some(task),
    })
}

async fn close(prompt: ChapterPrompt, text: &str, http: bool) -> Result<(Value, Vec<Value>)> {
    let server = serve(text, http).await?;
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path())?;
    let session = store.create_session("chapter prompt HTTP")?.id;
    let messages: Vec<Message> = (0..2).map(|index| {
        serde_json::from_value(json!({
            "id":format!("msg-{index}"), "session_id":session,
            "role":if index % 2 == 0 { "user" } else { "assistant" },
            "content":if index % 2 == 0 { "Fix the parser.".into() } else { "Parser source evidence. ".repeat(100) },
            "created_at":"2026-10-28T00:00:00Z"
        })).unwrap()
    }).collect();
    store.append_batch(&messages)?;
    let provider = Arc::new(NoMainCalls);
    let estimator = Arc::new(SummarizingContext::new(
        store.clone(),
        provider.clone(),
        SummaryConfig::default(),
        None,
    )?);
    let config = EpisodeConfig {
        summary_input: EpisodeSummaryInput::Episode,
        chapter_prompt: prompt,
        decider_enabled: false,
        source_target_tokens: 1,
        source_max_tokens: 4_000,
        chapter_rollover_tokens: 1,
        ..Default::default()
    };
    let mut registry = ProviderRegistry::new("main", provider);
    if http {
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
        )?;
    } else {
        registry.insert(
            "opencode",
            Arc::new(ScriptedTransport {
                replies: server.replies.clone(),
                requests: server.requests.clone(),
            }),
        )?;
    }
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: ModelRoles::for_models("opencode/deepseek-v4.1-flash".into(), String::new()),
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )?
    .with_episode_config(config.clone())?;
    let worker =
        EpisodeWorker::new_with_config(store.clone(), Arc::new(models), estimator, config)?;
    let before = store.storage_stats()?.transaction_count;
    worker.precompute(&session).await?;
    let chapters = store.context_subject_chapters(&session)?;
    assert_eq!(chapters.len(), 1);
    assert_eq!(chapters[0].status, SubjectChapterStatus::Closed);
    assert_eq!(store.context_episodes(&session)?.len(), 1);
    // Starting the chapter call first persists the completed summary's usage.
    // The chapter's usage and both derived records then share one batch commit.
    assert_eq!(store.storage_stats()?.transaction_count, before + 2);
    let usage = store.request_usage()?;
    assert_eq!(usage.len(), 2);
    assert!(usage.iter().all(|row| {
        row.kind == bubblepaw_core::RequestKind::SummaryFork
            && !row.failed
            && row.usage.input_tokens == 19
            && row.usage.output_tokens == 7
    }));
    assert_eq!(
        serde_json::to_value(store.messages(&session)?)?,
        serde_json::to_value(messages)?
    );
    assert!(store.context_view(&session)?.is_none());
    assert!(
        chapters[0]
            .summary
            .as_deref()
            .unwrap()
            .contains("Parser fix passed at source msg-1.")
    );
    let connection = rusqlite::Connection::open(store.chat_path(&session)?.unwrap())?;
    let data: String = connection.query_row(
        "SELECT data FROM context_subject_chapters WHERE session_id=?1 AND status='closed'",
        [&session],
        |row| row.get(0),
    )?;
    assert!(
        data.len() <= 4_096,
        "chapter record grew to {} bytes",
        data.len()
    );
    assert!(server.replies.lock().unwrap().is_empty());
    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    Ok((serde_json::from_str(&data)?, requests))
}

const BODY: &str = "# Parser implementation\nParser fix passed at source msg-1.";

#[tokio::test]
async fn v1_chapter_keeps_existing_shape_and_v2_persists_retrieval_fields() -> Result<()> {
    let (v1, requests_v1) =
        close(ChapterPrompt::V1, &format!("{BODY}\nRelated: none"), true).await?;
    assert!(v1.get("decisions").is_none());
    assert!(v1.get("sections").is_none());
    let (v2, requests_v2) = close(ChapterPrompt::V2, &format!(
        "{BODY}\n\nDecisions:\n- Keep the parser fix.\n\nSections:\n- Implementation | episodes 1-1\nRelated: none"
    ), true).await?;
    assert_eq!(v2["decisions"], json!(["Keep the parser fix."]));
    assert_eq!(
        v2["sections"],
        json!([{"name":"Implementation","first_episode":1,"last_episode":1}])
    );
    assert_eq!(v1["summary"], v2["summary"]);
    let v1_messages = requests_v1[1]["messages"].as_array().unwrap();
    let v2_messages = requests_v2[1]["messages"].as_array().unwrap();
    assert_eq!(
        v1_messages.last(),
        v2_messages.last(),
        "episode evidence changed with prompt version"
    );
    assert_ne!(v1_messages[0]["content"], v2_messages[0]["content"]);
    Ok(())
}

#[tokio::test]
async fn invalid_v2_retrieval_fields_keep_memory_without_retry_or_extra_commit() -> Result<()> {
    let captured = logs::capture_warnings();
    let (data, _) = close(ChapterPrompt::V2, &format!(
        "{BODY}\n\nDecisions:\n- Keep the parser fix.\n\nSections:\n- Implementation | episodes 2-2\nRelated: none"
    ), true).await?;
    assert!(data.get("decisions").is_none());
    assert!(data.get("sections").is_none());
    let logged = String::from_utf8(captured.lock().unwrap().clone())?;
    let warnings: Vec<_> = logged
        .lines()
        .filter(|line| line.contains("chapter_metadata_invalid"))
        .collect();
    assert_eq!(warnings.len(), 1, "{logged}");
    assert!(warnings[0].contains("WARN"), "{logged}");
    assert!(warnings[0].contains("section_coverage"), "{logged}");
    assert!(
        !warnings[0].contains("Implementation"),
        "metadata text entered warning: {logged}"
    );
    Ok(())
}

#[tokio::test]
async fn scripted_transport_closes_v1_and_v2_chapters_with_the_expected_stored_shape() -> Result<()>
{
    for prompt in [ChapterPrompt::V1, ChapterPrompt::V2] {
        let text = if prompt == ChapterPrompt::V2 {
            format!(
                "{BODY}\n\nDecisions: none\n\nSections:\n- Implementation | episodes 1-1\nRelated: none"
            )
        } else {
            format!("{BODY}\nRelated: none")
        };
        let (data, _) = close(prompt, &text, false).await?;
        if prompt == ChapterPrompt::V2 {
            assert_eq!(data["decisions"], json!([]));
            assert_eq!(
                data["sections"],
                json!([{"name":"Implementation","first_episode":1,"last_episode":1}])
            );
        } else {
            assert!(data.get("decisions").is_none());
            assert!(data.get("sections").is_none());
        }
    }
    Ok(())
}
