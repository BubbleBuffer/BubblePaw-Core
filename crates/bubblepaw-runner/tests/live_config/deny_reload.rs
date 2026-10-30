use super::*;

#[tokio::test]
async fn deny_rules_reload_and_survive_run_override() {
    let fixture = Fixture::new(&config("")).await;
    fixture.turn("initial deny config").await;
    fixture.write(&config("").replace("[approvals]", "[approvals]\ndeny=['exec']"));
    fixture.code("denied override", "try { await tools.exec({command:'printf forbidden'}); throw Error('deny bypassed'); } catch(e) { if(!String(e).includes('deny rule exec')) throw e; } return 'denied';");
    let run = fixture
        .harness
        .prompt(
            &fixture.session,
            "denied override",
            RunOptions {
                approval: Some(bubblepaw_core::ApprovalPolicy {
                    confirm: bubblepaw_core::ApprovalMode::Allow,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(run.status, "completed");
    assert!(
        fixture
            .store
            .messages(&fixture.session)
            .unwrap()
            .iter()
            .all(|m| !m.is_error)
    );
    assert_eq!(fixture.live.approval().deny, ["exec"]);
    assert!(
        fixture
            .notices()
            .iter()
            .any(|notice| notice.contains("approval deny rules") && notice.contains("exec"))
    );
    fixture.write(&config(""));
    let reload = fixture.live.reload(None).await;
    assert!(reload.applied);
    assert!(!reload.restart_needed);
    assert!(fixture.live.approval().deny.is_empty());
    fixture.code(
        "allowed after removal",
        "return await tools.exec({command:'printf allowed'});",
    );
    fixture.turn("allowed after removal").await;
    fixture.shutdown().await;
}
