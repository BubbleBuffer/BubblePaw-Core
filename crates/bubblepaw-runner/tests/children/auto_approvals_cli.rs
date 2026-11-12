//! Automatic approval wiring through the real one-shot runner and a local LLM.
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn one_shot_auto_approves_or_falls_through_to_unattended_denial() -> anyhow::Result<()> {
    for (answer, approved) in [
        ("{\"approve\":true}", true),
        ("{\"approve\":false}", false),
        ("malformed", false),
    ] {
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = requests.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body);
                Json(json!({"choices":[{"message":{"content":answer}}],"usage":{"prompt_tokens":12,"completion_tokens":4}}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}/v1", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let root = tempfile::tempdir()?;
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
            .args(["--provider", "demo", "--json", "--workspace"])
            .arg(root.path())
            .arg("--db")
            .arg(root.path().join("archive.sqlite"))
            .args([
                "--read-only",
                "--no-auto-summarize",
                "--confirm-auto",
                "list",
                "--approver",
                "llm/opencode/mock",
                "--episode-summary-base-url",
            ])
            .arg(base)
            .args([
                "--summary-model",
                "mock",
                "--episode-summary-key-env",
                "BUBBLEPAW_TEST_ROLE_KEY",
                "run",
                "Run the demo",
            ])
            .env("BUBBLEPAW_TEST_ROLE_KEY", "local-fixture-key")
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .output()
            .await?;
        server.abort();
        let stdout = String::from_utf8(output.stdout)?;
        assert_eq!(
            output.status.success(),
            approved,
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if approved {
            assert!(stdout.contains("\"state\":\"approved\""), "{stdout}");
            assert!(stdout.contains("auto llm/opencode/mock p=1"), "{stdout}");
            assert!(!stdout.contains("no client to approve"), "{stdout}");
        } else {
            assert!(stdout.contains("\"state\":\"denied\""), "{stdout}");
            assert!(
                stdout.contains("no client to approve; use --approve-all"),
                "{stdout}"
            );
        }
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["model"], "mock");
        assert_eq!(requests[0]["reasoning_effort"], "low");
        // The default approval prompt, rendered with this call's fields.
        let input = requests[0]["messages"][1]["content"].as_str().unwrap();
        assert!(input.contains("User request:\nRun the demo"), "{input}");
        assert!(input.contains("Tool: list"), "{input}");
        assert!(!input.contains("{description}"), "{input}");
        assert!(!input.contains("{working_directory}"), "{input}");
    }
    Ok(())
}
