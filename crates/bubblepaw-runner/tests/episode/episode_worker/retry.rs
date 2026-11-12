use super::*;

#[tokio::test]
async fn automatic_wakes_deduplicate_failed_range_until_revision_or_confirmed_route_changes()
-> Result<()> {
    let fixture = Fixture::new(3).await?;
    let models = Arc::new(Models::default());
    models.fail.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models.clone());
    let begin = || -> Result<()> {
        fixture.estimator.observe_request(
            &fixture.session,
            &ModelRequest {
                model: "logical-main".into(),
                messages: fixture.store.messages(&fixture.session)?,
                ..Default::default()
            },
            Arc::new(MainProvider),
        );
        Ok(())
    };
    let confirm = |route: &str| {
        fixture.estimator.observe_response(
            &fixture.session,
            &ModelResponse {
                provider_state: Some(serde_json::json!({"_bubblepaw_selected_model":route})),
                ..Default::default()
            },
        )
    };
    begin()?;
    confirm("free/model");
    worker.trigger(&fixture.session, 0, 0);
    wait(&worker, &fixture.session).await?;
    assert_eq!(models.summaries.lock().unwrap().len(), 1);
    for index in 0..3 {
        fixture
            .harness
            .prompt(
                &fixture.session,
                format!("new main round {index}"),
                RunOptions::default(),
            )
            .await?;
        begin()?;
        confirm("free/model");
        worker.trigger(&fixture.session, 0, 0);
        wait(&worker, &fixture.session).await?;
    }
    assert_eq!(models.summaries.lock().unwrap().len(), 1);
    assert!(
        fixture
            .store
            .episode_coverage_token(&fixture.session)?
            .is_none()
    );
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    models.revision.store(1, Ordering::SeqCst);
    worker.trigger(&fixture.session, 0, 0);
    wait(&worker, &fixture.session).await?;
    assert_eq!(models.summaries.lock().unwrap().len(), 2);
    // Releasing a warm prefix between runs does not confirm a route change.
    fixture.estimator.release_session(&fixture.session);
    begin()?;
    worker.trigger(&fixture.session, 0, 0);
    wait(&worker, &fixture.session).await?;
    assert_eq!(models.summaries.lock().unwrap().len(), 2);
    confirm("free/model");
    // A pending request still belongs to the prior confirmed route.
    begin()?;
    worker.trigger(&fixture.session, 0, 0);
    wait(&worker, &fixture.session).await?;
    assert_eq!(models.summaries.lock().unwrap().len(), 2);
    confirm("go/model");
    worker.trigger(&fixture.session, 0, 0);
    wait(&worker, &fixture.session).await?;
    assert_eq!(models.summaries.lock().unwrap().len(), 3);
    models.fail.store(false, Ordering::SeqCst);
    worker
        .gather_pressure_step(
            &fixture.session,
            fixture.store.messages(&fixture.session)?.len(),
            false,
            None,
        )
        .await?;
    assert_eq!(models.summaries.lock().unwrap().len(), 4);
    assert_eq!(fixture.store.context_episodes(&fixture.session)?.len(), 1);
    // A successful next coverage removes the suppression key entirely.
    worker.trigger(&fixture.session, 0, 0);
    wait(&worker, &fixture.session).await?;
    assert!(fixture.store.context_episodes(&fixture.session)?.len() > 1);
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    Ok(())
}
