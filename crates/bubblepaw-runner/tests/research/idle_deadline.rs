use super::*;

#[tokio::test]
async fn ordinary_research_child_wait_can_outlive_the_js_idle_deadline() {
    let probe = Arc::new(Probe {
        block: true,
        ..Default::default()
    });
    let fixture = Fixture::with_config(
        probe.clone(),
        WebConfig::default(),
        HarnessConfig {
            model: "parent/model".into(),
            ..Default::default()
        },
        None,
        Duration::from_secs(1),
    );
    fixture
        .parent
        .start(
            &fixture.session,
            "long research wait",
            RunOptions {
                script: Some(
                    "const child=await tools.agent({task:'Wait past the idle limit',profile:'research'}); return await wait({handle:child.handle});".into(),
                ),
                ..Default::default()
            },
        )
        .unwrap();
    wait_active(&probe, 1).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    probe.release.notify_waiters();
    let run = fixture
        .parent
        .wait_for_idle(&fixture.session)
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let result: Value = serde_json::from_str(run.text.as_deref().unwrap()).unwrap();
    assert_eq!(result["status"], "completed");
    fixture.shutdown().await;
}
