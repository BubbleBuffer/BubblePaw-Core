use super::*;

#[tokio::test]
async fn stdio_can_cancel_an_outstanding_provider_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (entered_tx, mut entered) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/responses",
            axum::routing::post(move || {
                let entered_tx = entered_tx.clone();
                async move {
                    let _ = entered_tx.send(());
                    std::future::pending::<()>().await;
                    "never completed"
                }
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(fixture_binary::resolve(
        env!("CARGO_BIN_EXE_bubblepaw"),
        "bubblepaw",
    ))
    .args([
        "--provider",
        "api",
        "--base-url",
        &format!("http://{address}"),
        "--decider",
        "llm/api/gpt-6-luna",
        "--workspace",
    ])
    .arg(temp.path())
    .arg("--db")
    .arg(temp.path().join("archive.sqlite"))
    .arg("stdio")
    .env("OPENAI_API_KEY", "synthetic-fixture")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(&mut stdin, 1, "session.open", json!({})).await;
    let session = response(&mut stdout, 1).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &mut stdin,
        2,
        "run.start",
        json!({"session_id":session,"text":"Wait for provider"}),
    )
    .await;
    response(&mut stdout, 2).await;
    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .expect("local /responses handler was not entered before cancellation")
        .expect("local provider fixture stopped before receiving the request");
    send(&mut stdin, 3, "run.cancel", json!({"session_id":session})).await;
    assert_eq!(response(&mut stdout, 3).await["requested"], true);
    send(&mut stdin, 4, "shutdown", json!({})).await;
    response(&mut stdout, 4).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(4), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    server.abort();
}
