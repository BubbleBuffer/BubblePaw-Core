use super::*;

#[tokio::test]
async fn http_steer_then_explicit_interrupt_preserves_wait_and_captured_outputs_across_restart()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("seed.txt"), "original captured contents")?;
    let gif = [
        71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 0, 0, 0, 255, 255, 255, 33, 249, 4, 1, 0, 0,
        0, 0, 44, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 68, 1, 0, 59,
    ];
    std::fs::write(root.join("pixel.gif"), gif)?;
    let assets = directory.path().join("dist");
    std::fs::create_dir(&assets)?;
    std::fs::write(assets.join("index.html"), "<html>fixture frontend</html>")?;
    let path = directory.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("CodeMode HTTP")?;
    let provider = Arc::new(Capture {
        inner: ScriptedProvider::with_steps(
            vec![
                js(
                    "cell-main",
                    "main",
                    "await tools.environments({action:'create',environment:'diagnostic'}); state.count=1; const seed=await tools.read({path:'seed.txt'}); text(seed.content); await file({path:'seed.txt'}); await image({path:'pixel.gif',alt:'one pixel'}); await wait({ms:60000}); state.count=2; text('wait resumed');",
                ),
                js(
                    "env-inspect",
                    "diagnostic",
                    "await tools.environments({action:'inspect',environment:'main'});",
                ),
                js(
                    "cell-diagnostic",
                    "diagnostic",
                    "text('diagnostic investigation');",
                ),
            ],
            "foreground completed",
        ),
        requests: Mutex::default(),
    });
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, &root, false)?;
    let artifacts = Arc::new(ArtifactStore::new(
        &root,
        directory.path().join("artifacts"),
    )?);
    let repl = Arc::new(CodeMode::new(root.clone(), artifacts.clone())?);
    repl.register(&mut tools)?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        tools.clone(),
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let config = WebConfig {
        workspace: root.display().to_string(),
        model: "fixture".into(),
        provider: "demo".into(),
        read_only: false,
    };
    let (stop, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve_with_services(
        listener,
        harness.clone(),
        store.clone(),
        config,
        assets,
        WebServices {
            repl: Some(repl.clone()),
            artifacts: Some(artifacts.clone()),
            input_files: None,
            ..Default::default()
        },
        async {
            let _ = receive.await;
        },
    ));
    let client = reqwest::Client::new();
    let url = format!("{base}/api/sessions/{}", session.id);
    client
        .post(format!("{url}/run"))
        .json(&json!({"text":"start"}))
        .send()
        .await?
        .error_for_status()?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value: Value = client
                .get(format!("{url}/environments"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if value["environments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|environment| {
                    environment["name"] == "main"
                        && environment["waiting"].as_u64().unwrap_or(0) > 0
                })
            {
                break Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    client
        .post(format!("{url}/steer"))
        .json(&json!({"text":"Why is this taking so long? Investigate."}))
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(
        repl.status(&session.id)["environments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|environment| {
                environment["name"] == "main"
                    && environment["waiting"].as_u64().unwrap_or(0) > 0
                    && environment["status"] != "background"
            })
    );
    assert!(tools.interrupt_waits(&session.id) > 0);
    tokio::time::timeout(Duration::from_secs(5), harness.wait_for_idle(&session.id)).await??;
    let state: Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        state["repl"]["environments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "main" && e["status"] == "background")
    );
    assert!(
        provider.requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|m| m.content.contains("Why is this taking so long?"))
    );
    assert!(
        state["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["output"]["text"] == "diagnostic investigation")
    );
    let attachment = state["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["output"]["type"] == "file")
        .unwrap()["output"]["artifact"]
        .clone();
    std::fs::write(root.join("seed.txt"), "later changed")?;
    let response = client
        .get(format!(
            "{url}/artifacts/{}",
            attachment["id"].as_str().unwrap()
        ))
        .send()
        .await?
        .error_for_status()?;
    assert!(
        response.headers()["content-disposition"]
            .to_str()?
            .starts_with("attachment")
    );
    assert_eq!(response.text().await?, "original captured contents");
    let outsider = store.create_session("other session")?;
    assert_eq!(
        client
            .get(format!(
                "{base}/api/sessions/{}/artifacts/{}",
                outsider.id,
                attachment["id"].as_str().unwrap()
            ))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    client
        .post(format!("{url}/environments"))
        .json(&json!({"action":"wake","environment":"main"}))
        .send()
        .await?
        .error_for_status()?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value: Value = client
                .get(&url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if value["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["output"]["text"] == "wait resumed")
                && value["repl"]["environments"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|env| env["status"] == "idle")
                && store.pending_effects(&session.id)?.is_empty()
            {
                break Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let before = store.revision();
    for _ in 0..3 {
        client.get(&url).send().await?.error_for_status()?;
    }
    assert_eq!(store.revision(), before);
    let _ = stop.send(());
    task.await??;
    repl.shutdown().await;
    processes.shutdown().await;
    drop(harness);
    drop(store);
    let reopened = Store::open(&path)?;
    assert!(
        reopened
            .outputs(&session.id, 128)?
            .iter()
            .any(|o| o.output["type"] == "image")
    );
    assert!(
        reopened
            .outputs(&session.id, 128)?
            .iter()
            .any(|o| o.output["text"] == "wait resumed")
    );
    let captured = ArtifactStore::new(&root, directory.path().join("artifacts"))?;
    assert_eq!(
        captured
            .read(&session.id, attachment["id"].as_str().unwrap())?
            .1,
        b"original captured contents"
    );
    Ok(())
}
