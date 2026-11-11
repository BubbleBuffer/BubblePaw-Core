use super::*;

struct UnavailableRoute(std::sync::atomic::AtomicUsize);
#[async_trait]
impl Provider for UnavailableRoute {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(bubblepaw_core::ProviderError::new(
            bubblepaw_core::ProviderErrorKind::Auth,
            "free route unavailable",
        )
        .into())
    }
}

#[derive(Default)]
struct PendingGo {
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl Provider for PendingGo {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call == 1 {
            self.entered.notify_one();
            self.release.notified().await;
        } else if call > 1 {
            return Err(bubblepaw_core::ProviderError::new(
                bubblepaw_core::ProviderErrorKind::Fatal,
                "failed later Go call",
            )
            .into());
        }
        Ok(ModelResponse {
            text: format!("completed Go response {call}"),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn pending_sticky_go_main_keeps_completed_route_and_failed_main_cannot_promote() -> Result<()>
{
    let f = Fixture::new()?;
    let free = Arc::new(UnavailableRoute(std::sync::atomic::AtomicUsize::new(0)));
    let go = Arc::new(PendingGo::default());
    let mut registry = bubblepaw_runner::provider::ProviderRegistry::new("free", free.clone());
    registry.insert("go", go.clone())?;
    registry.set_model_fallbacks("free/main".into(), vec!["go/main".into()])?;
    let registry = Arc::new(registry);
    let context = Arc::new(
        SummarizingContext::new(
            f.store.clone(),
            registry.clone(),
            SummaryConfig::default(),
            None,
        )?
        .with_auto_summarize(false),
    );
    let harness = Harness::new(
        f.store.clone(),
        registry.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            model: "free/main".into(),
            context: context.clone(),
            ..Default::default()
        },
    );
    assert_eq!(context.episode_fork_route(&f.session), None);
    let first_run = harness
        .prompt(
            &f.session,
            "first task",
            RunOptions {
                system_prompt: Some("first warmed system".into()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(first_run.status, "completed");
    let source = f.store.messages(&f.session)?;
    let first = source.first().unwrap().id.clone();
    let first_end = source.last().unwrap().id.clone();
    let first_fork = context
        .episode_fork(&f.session, &first, &first_end)?
        .unwrap();
    assert_eq!(first_fork.request.model, "go/main");
    assert_eq!(
        context.episode_fork_route(&f.session).as_deref(),
        Some("go/main")
    );

    let next_harness = harness.clone();
    let session = f.session.clone();
    let next = tokio::spawn(async move {
        next_harness
            .prompt(
                &session,
                "next pending task",
                RunOptions {
                    system_prompt: Some("second warmed system".into()),
                    ..Default::default()
                },
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), go.entered.notified()).await?;
    // The next request logically names free/main, but registry sticky routing
    // has already selected Go. Its unconfirmed capture cannot replace the last
    // completed Go prefix while the main HTTP-equivalent call is blocked.
    let pending = context
        .episode_fork(&f.session, &first, &first_end)?
        .unwrap();
    assert_eq!(pending.request.model, "go/main");
    assert_eq!(
        context.episode_fork_route(&f.session).as_deref(),
        Some("go/main")
    );
    assert_eq!(pending.request.system_prompt, "first warmed system");
    assert_eq!(pending.request.messages, source);
    assert!(Arc::ptr_eq(
        &pending.provider,
        &(registry.clone() as Arc<dyn Provider>)
    ));
    assert_eq!(free.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    go.release.notify_one();
    assert_eq!(next.await??.status, "completed");

    let source = f.store.messages(&f.session)?;
    let completed_end = source.last().unwrap().id.clone();
    let completed = context
        .episode_fork(&f.session, &first, &completed_end)?
        .unwrap();
    assert_eq!(completed.request.model, "go/main");
    // A later RunOptions override cannot replace this chat's frozen prefix.
    assert_eq!(
        completed.request.system_prompt,
        first_fork.request.system_prompt
    );
    assert_eq!(completed.request.messages, source);
    assert!(completed.request.messages.len() > pending.request.messages.len());
    assert!(
        completed
            .request
            .messages
            .iter()
            .any(|message| message.content == "next pending task"),
        "the latest successful capture advances history despite its unchanged system prompt"
    );
    let failed = harness
        .prompt(
            &f.session,
            "failed task",
            RunOptions {
                system_prompt: Some("unconfirmed failed system".into()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(failed.status, "failed");
    let retained = context
        .episode_fork(&f.session, &first, &completed_end)?
        .unwrap();
    assert_eq!(retained.request.model, "go/main");
    assert_eq!(
        context.episode_fork_route(&f.session).as_deref(),
        Some("go/main")
    );
    assert_eq!(
        retained.request.system_prompt,
        completed.request.system_prompt
    );
    assert_eq!(retained.request.messages, completed.request.messages);
    assert_eq!(free.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    Ok(())
}
