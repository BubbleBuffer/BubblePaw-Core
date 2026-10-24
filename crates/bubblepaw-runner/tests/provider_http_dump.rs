//! Local HTTP only; environment changes are isolated to a test subprocess.
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, RequestKind, Role, capture_request_usage,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const CHILD: &str = "BP_DUMP_TEST_CHILD";
const DIRECTORY: &str = "BP_DUMP_TEST_DIRECTORY";
const DUMP_ENV: &str = "BUBBLEPAW_PROVIDER_DUMP_DIR";
const TOKEN: &str = "synthetic-auth-token-never-dump";
const RESPONSE_HEADER: &str = "synthetic-response-header-never-dump";
type Captured = Arc<Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;

#[test]
fn raw_http_dumps_local_transports() {
    if let Ok(mode) = std::env::var(CHILD) {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(&mode));
        return;
    }
    for mode in ["enabled", "disabled", "unwritable"] {
        let directory = tempfile::tempdir().unwrap();
        let dump = directory.path().join("dumps");
        if mode == "unwritable" {
            std::fs::write(&dump, "a file, not a directory").unwrap();
        }
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "raw_http_dumps_local_transports", "--nocapture"])
            .env_clear()
            .env(CHILD, mode)
            .env(DIRECTORY, &dump);
        if mode != "disabled" {
            command.env(DUMP_ENV, &dump);
        }
        // Winsock requires SystemRoot even for loopback HTTP.
        #[cfg(windows)]
        for key in ["SystemRoot", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{mode}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn reply(model: &str) -> (StatusCode, &'static str, &'static str) {
    match model {
        "chat_sse" => (
            StatusCode::OK,
            "text/event-stream",
            concat!(
                ": provider comment retained\r\n\r\n",
                "event: message\r\ndata: {\"choices\":[{\"delta\":{\"discarded_reasoning\":\"private raw thought\",\"reasoning_content\":\"\u{03c0} plan\"}}]}\r\n\r\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
                ": another comment\n\ndata: [DONE]\n\n"
            ),
        ),
        "chat_partial" => (
            StatusCode::OK,
            "text/event-stream",
            concat!(
                ": incomplete stream\r\n\r\n",
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"unfinished thought\"}}]}\r\n\r\n",
                "data: {\"choices\":"
            ),
        ),
        "chat_error" => (
            StatusCode::BAD_REQUEST,
            "application/json",
            " {\n \"error\": {\"message\": \"synthetic rejection\"}, \"raw_reasoning\": \"rejected thought\"\n}\n",
        ),
        "messages_json" => (
            StatusCode::OK,
            "application/json",
            " {\n\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1},\"ignored_reasoning\":\"raw messages thought\"}\n",
        ),
        "responses_sse" => (
            StatusCode::OK,
            "text/event-stream",
            concat!(
                ": responses comment\r\n\r\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\r\n\r\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\r\n\r\n"
            ),
        ),
        "chat_cancel" => (
            StatusCode::OK,
            "text/event-stream",
            ": cancellation prefix\n\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"cancelled raw thought\"}}]}\n\ndata: {",
        ),
        _ => (
            StatusCode::OK,
            "application/json",
            " {\n \"choices\": [{\"message\": {\"content\": \"ok\", \"ignored_reasoning\": \"raw JSON thought\"}, \"finish_reason\": \"stop\"}], \"usage\": {\"prompt_tokens\":2, \"completion_tokens\":1}\n}\n",
        ),
    }
}

async fn handle(State(captured): State<Captured>, headers: HeaderMap, bytes: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let model = body["model"].as_str().unwrap().to_owned();
    captured
        .lock()
        .unwrap()
        .push((model.clone(), headers, bytes.to_vec()));
    let (status, mime, raw) = reply(&model);
    let body = if model == "chat_cancel" {
        let prefix = futures_util::stream::once(async move {
            Ok::<_, std::io::Error>(Bytes::from_static(raw.as_bytes()))
        });
        Body::from_stream(prefix.chain(futures_util::stream::pending()))
    } else {
        // Split UTF-8 and SSE framing across chunks.
        let chunks: Vec<_> = raw
            .as_bytes()
            .chunks(7)
            .map(|chunk| Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)))
            .collect();
        Body::from_stream(futures_util::stream::iter(chunks))
    };
    (
        status,
        [
            ("content-type", mime),
            ("x-private-fixture", RESPONSE_HEADER),
        ],
        body,
    )
        .into_response()
}

async fn run(mode: &str) {
    let directory = PathBuf::from(std::env::var_os(DIRECTORY).unwrap());
    let captured: Captured = Arc::default();
    let app = Router::new()
        .route("/chat/completions", post(handle))
        .route("/messages", post(handle))
        .route("/responses", post(handle))
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let chat: Arc<dyn Provider> = Arc::new(
        ChatCompletionsProvider::api_key(TOKEN, &base)
            .unwrap()
            .with_opencode_route(),
    );
    let messages: Arc<dyn Provider> = Arc::new(MessagesProvider::api_key(TOKEN, &base).unwrap());
    let responses: Arc<dyn Provider> = Arc::new(ResponsesProvider::api_key(TOKEN, &base).unwrap());
    let models = [
        "chat_sse",
        "chat_json",
        "chat_error",
        "chat_partial",
        "messages_json",
        "responses_sse",
    ];
    let futures = models.iter().map(|model| {
        let provider = match *model {
            "messages_json" => messages.clone(),
            "responses_sse" => responses.clone(),
            _ => chat.clone(),
        };
        async move {
            let (result, _) = capture_request_usage(
                "dump-fixture",
                None,
                RequestKind::SummaryFork,
                provider.generate(request(model), Arc::new(|_| {}), CancellationToken::new()),
            )
            .await;
            if matches!(*model, "chat_error" | "chat_partial") {
                assert!(result.is_err(), "{model}");
            } else {
                assert_eq!(result.unwrap().text, "ok", "{model}");
            }
        }
    });
    futures_util::future::join_all(futures).await;
    if mode == "enabled" {
        verify_pairs(&directory, &captured, "summary_fork", models.len());
        cancellation_prefix(&directory, chat).await;
        verify_pairs(&directory, &captured, "", models.len() + 1);
    } else if mode == "disabled" {
        assert!(!directory.exists(), "disabled dumping created a directory");
    } else {
        assert_eq!(
            std::fs::read_to_string(&directory).unwrap(),
            "a file, not a directory"
        );
    }
    server.abort();
}

fn request(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        system_prompt: "Synthetic prompt \u{03c0}".into(),
        messages: vec![Message {
            id: "synthetic-user-message".into(),
            session_id: "synthetic-session-header".into(),
            role: Role::User,
            origin: None,
            content: "Return the synthetic fixture response.".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        }],
        session_id: "synthetic-session-header".into(),
        max_output_tokens: Some(100),
        ..Default::default()
    }
}

fn verify_pairs(directory: &Path, captured: &Captured, kind: &str, expected: usize) {
    let files: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), expected * 2);
    let requests = captured.lock().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for path in files
        .iter()
        .filter(|path| path.to_string_lossy().ends_with(".request.json"))
    {
        let name = path.file_name().unwrap().to_str().unwrap();
        if !kind.is_empty() {
            assert!(name.contains(&format!("-{kind}-")), "{name}");
        }
        let bytes = std::fs::read(path).unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let model = body["model"].as_str().unwrap();
        assert!(
            seen.insert(model.to_owned()),
            "duplicate physical pair for {model}"
        );
        let (_, headers, wire) = requests.iter().find(|(name, _, _)| name == model).unwrap();
        assert_eq!(&bytes, wire, "request bytes for {model}");
        if model == "messages_json" {
            assert_eq!(headers["x-api-key"], TOKEN);
        } else {
            assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
        }
        let response_path = path.with_file_name(name.replace(".request.json", ".response.txt"));
        let response = std::fs::read(&response_path).unwrap();
        assert_eq!(
            response,
            reply(model).2.as_bytes(),
            "response bytes for {model}"
        );
        for content in [&bytes, &response] {
            let text = String::from_utf8_lossy(content);
            for excluded in [
                TOKEN,
                RESPONSE_HEADER,
                "synthetic-session-header",
                "Authorization:",
                "x-api-key",
            ] {
                assert!(!text.contains(excluded), "header material in {name}");
            }
        }
    }
    assert_eq!(seen.len(), expected);
}

async fn cancellation_prefix(directory: &Path, chat: Arc<dyn Provider>) {
    let token = CancellationToken::new();
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        chat.generate(request("chat_cancel"), Arc::new(|_| {}), task_token)
            .await
    });
    // Wait for disk evidence that the prefix arrived before cancelling.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let arrived = std::fs::read_dir(directory).unwrap().any(|entry| {
                let path = entry.unwrap().path();
                path.to_string_lossy().ends_with(".response.txt")
                    && std::fs::read(path).unwrap() == reply("chat_cancel").2.as_bytes()
            });
            if arrived {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    token.cancel();
    assert!(task.await.unwrap().is_err());
}
