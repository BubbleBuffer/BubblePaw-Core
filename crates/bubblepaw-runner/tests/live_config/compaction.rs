//! Real host prefix lifecycle across LCM applies and host reconstruction.
use super::*;
use bubblepaw_core::Usage;
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, ChapterResult, ClassicalInput, ClassicalResult, EpisodeModels, SplitInput,
        SplitResult, SummaryInput, SummaryResult,
    },
    episode_worker::EpisodeWorker,
    summarization::{SummarizingContext, SummaryConfig},
};

#[path = "../support/episode_fixture.rs"]
mod episode_fixture;

struct SummaryModels;
#[async_trait]
impl EpisodeModels for SummaryModels {
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len() + 1];
        *probabilities.last_mut().unwrap() = 1.0;
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }

    async fn summarize(&self, _: SummaryInput) -> Result<SummaryResult> {
        Ok(SummaryResult {
            title: "Host lifecycle".into(),
            summary: "Earlier fixture turns completed.".into(),
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }

    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        anyhow::bail!("fixture has no subject changes")
    }

    async fn summarize_classical(&self, _: ClassicalInput) -> Result<ClassicalResult> {
        Ok(ClassicalResult {
            summary: "Earlier classical fixture turns completed.".into(),
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
}

fn install_compaction(fixture: &mut Fixture) -> Arc<EpisodeWorker> {
    install_context(fixture, SummaryConfig::default(), false, None)
}

struct ChangeAfterRefresh {
    inner: Arc<dyn bubblepaw_core::TurnStart>,
    files: Mutex<Option<Vec<(PathBuf, String)>>>,
}

#[async_trait]
impl bubblepaw_core::TurnStart for ChangeAfterRefresh {
    async fn before_turn(&self, session: &str, cancellation: CancellationToken) -> Result<()> {
        self.inner.before_turn(session, cancellation).await?;
        if let Some(files) = self.files.lock().unwrap().take() {
            for (path, content) in files {
                std::fs::write(path, content)?;
            }
        }
        Ok(())
    }
}

fn install_context(
    fixture: &mut Fixture,
    config: SummaryConfig,
    automatic: bool,
    after_refresh: Option<Vec<(PathBuf, String)>>,
) -> Arc<EpisodeWorker> {
    let context = Arc::new(
        SummarizingContext::new(fixture.store.clone(), fixture.script.clone(), config, None)
            .unwrap()
            .with_auto_summarize(automatic)
            .with_surface_refresh(fixture.live.surface_refresh()),
    );
    let worker = episode_fixture::new_episode_worker(
        fixture.store.clone(),
        Arc::new(SummaryModels),
        context.clone(),
    );
    context.set_episode_worker(&worker);
    let mut harness_config = HarnessConfig {
        execution_mode: bubblepaw_core::ExecutionMode::Js,
        model: "demo/first".into(),
        system_prompt: "BASE-COMPACTION-INSTRUCTIONS".into(),
        context,
        ..Default::default()
    };
    let providers = fixture.providers.clone();
    let script = fixture.script.clone();
    harness_config.provider_resolver =
        Some(Arc::new(move || (providers.main_model(), script.clone())));
    fixture.live.configure_harness(&mut harness_config);
    if let Some(files) = after_refresh {
        harness_config.turn_start = Some(Arc::new(ChangeAfterRefresh {
            inner: harness_config.turn_start.take().unwrap(),
            files: Mutex::new(Some(files)),
        }));
    }
    fixture.harness = Arc::new(Harness::new(
        fixture.store.clone(),
        fixture.script.clone(),
        fixture.live.registry(),
        harness_config,
    ));
    worker
}

fn skill(fixture: &Fixture, name: &str, description: &str) {
    let directory = fixture._root.path().join(".agents/skills").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nBODY-IS-NOT-PREFIX\n"),
    )
    .unwrap();
}

fn assert_served_prefix(fixture: &Fixture, expected: &ModelRequest) {
    let requests = fixture.script.requests.lock().unwrap();
    let served = requests
        .last()
        .expect("scripted provider received a request");
    assert_eq!(served.system_prompt, expected.system_prompt);
    assert_eq!(
        serde_json::to_vec(&served.tools).unwrap(),
        serde_json::to_vec(&expected.tools).unwrap()
    );
}

fn host_notices(fixture: &Fixture) -> Vec<bubblepaw_core::Message> {
    fixture
        .store
        .messages(&fixture.session)
        .unwrap()
        .into_iter()
        .filter(|message| {
            message.origin == Some(MessageOrigin::Generated)
                && (message.content.contains("Memory files changed")
                    || message.content.contains("Skills changed"))
        })
        .collect()
}

async fn reopen(fixture: Fixture) -> Fixture {
    fixture.shutdown().await;
    let Fixture {
        _root,
        file,
        store,
        live,
        providers,
        mcp,
        processes,
        agents,
        harness,
        script,
        session,
    } = fixture;
    drop((
        file, store, live, providers, mcp, processes, agents, harness, script,
    ));
    Fixture::open(_root, &config(""), true, Some(session)).await
}

#[tokio::test]
async fn lcm_apply_rebinds_current_host_prefix_and_reopen_preserves_latest_snapshot() {
    let mut fixture = Fixture::new_with_layout(&config(""), true).await;
    let mut worker = install_compaction(&mut fixture);
    let memory = fixture._root.path().join("AGENTS.md");
    std::fs::write(&memory, "MEMORY-ORIGINAL-V1").unwrap();
    skill(&fixture, "kept", "CATALOG-ORIGINAL-V1");
    skill(&fixture, "removed", "CATALOG-REMOVED");
    fixture.turn("initial host prefix").await;
    fixture.turn("retain a recent turn").await;
    let original = fixture.harness.request_template(&fixture.session).unwrap();
    let original_memory = fixture
        .store
        .attachment(&fixture.session, "bubblepaw-memory-files-v1")
        .unwrap();
    let original_skills = fixture
        .store
        .attachment(&fixture.session, "bubblepaw-skills-v1")
        .unwrap();
    assert!(original_memory.is_some() && original_skills.is_some());
    assert!(original.system_prompt.contains("MEMORY-ORIGINAL-V1"));
    assert!(original.system_prompt.contains("CATALOG-ORIGINAL-V1"));

    std::fs::write(&memory, "MEMORY-APPLIED-V2").unwrap();
    skill(&fixture, "kept", "CATALOG-APPLIED-V2");
    skill(&fixture, "newly-added", "CATALOG-NEWLY-ADDED-V2");
    let added_path = fixture
        ._root
        .path()
        .join(".agents/skills/newly-added/SKILL.md")
        .canonicalize()
        .unwrap();
    fixture.turn("announce current changes").await;
    assert!(
        host_notices(&fixture)
            .last()
            .unwrap()
            .content
            .contains("newly-added")
    );
    skill(&fixture, "roundtrip", "CATALOG-ROUNDTRIP-ABSENT-AT-APPLY");
    fixture.turn("announce added roundtrip skill").await;
    assert!(
        host_notices(&fixture)
            .last()
            .unwrap()
            .content
            .contains("roundtrip")
    );
    std::fs::remove_dir_all(fixture._root.path().join(".agents/skills/roundtrip")).unwrap();
    fixture.turn("announce removed roundtrip skill").await;
    let removed = host_notices(&fixture).pop().unwrap();
    assert!(removed.content.contains("roundtrip") && removed.content.contains("removed"));
    fixture.turn("retain turn after every change notice").await;
    let covered_notices = host_notices(&fixture);
    assert_eq!(covered_notices.len(), 3);
    assert_eq!(
        fixture
            .harness
            .request_template(&fixture.session)
            .unwrap()
            .system_prompt,
        original.system_prompt
    );
    let history_before_apply = fixture.store.messages(&fixture.session).unwrap();
    let notice = fixture
        .harness
        .compact_session(&fixture.session, None)
        .await
        .unwrap()
        .unwrap();
    assert!(!notice.contains("Nothing to compact"), "{notice}");
    assert_eq!(
        fixture.store.messages(&fixture.session).unwrap(),
        history_before_apply
    );
    let applied = fixture.harness.request_template(&fixture.session).unwrap();
    assert!(
        applied
            .system_prompt
            .contains("BASE-COMPACTION-INSTRUCTIONS")
    );
    assert!(applied.system_prompt.contains("MEMORY-APPLIED-V2"));
    assert!(applied.system_prompt.contains("CATALOG-APPLIED-V2"));
    assert!(applied.system_prompt.contains("newly-added"));
    assert!(applied.system_prompt.contains("CATALOG-NEWLY-ADDED-V2"));
    assert!(
        applied
            .system_prompt
            .contains(&json!(added_path).to_string())
    );
    assert!(!applied.system_prompt.contains("MEMORY-ORIGINAL-V1"));
    assert!(!applied.system_prompt.contains("CATALOG-ORIGINAL-V1"));
    assert!(!applied.system_prompt.contains("BODY-IS-NOT-PREFIX"));
    assert!(
        !applied
            .system_prompt
            .contains("CATALOG-ROUNDTRIP-ABSENT-AT-APPLY")
    );
    let frontier = fixture
        .store
        .context_view(&fixture.session)
        .unwrap()
        .unwrap()
        .frontier
        .unwrap();
    for message in &covered_notices {
        assert!(
            fixture
                .store
                .message_seq(&fixture.session, &message.id)
                .unwrap()
                <= frontier
        );
    }
    assert_eq!(
        serde_json::to_vec(&applied.tools).unwrap(),
        serde_json::to_vec(&original.tools).unwrap()
    );
    assert_eq!(
        fixture
            .store
            .attachment(&fixture.session, "bubblepaw-memory-files-v1")
            .unwrap(),
        original_memory
    );
    assert_eq!(
        fixture
            .store
            .attachment(&fixture.session, "bubblepaw-skills-v1")
            .unwrap(),
        original_skills
    );
    let notice_count = host_notices(&fixture).len();
    fixture.turn("stable after apply").await;
    assert_served_prefix(&fixture, &applied);
    {
        let requests = fixture.script.requests.lock().unwrap();
        let served = requests.last().unwrap();
        for message in &served.messages {
            assert!(
                !covered_notices
                    .iter()
                    .any(|covered| covered.id == message.id)
            );
            assert!(!message.content.contains("Memory files changed"));
            assert!(!message.content.contains("Skills changed"));
        }
    }
    assert_eq!(
        host_notices(&fixture).len(),
        notice_count,
        "rebinding must absorb already announced changes"
    );
    assert_eq!(
        fixture
            .harness
            .request_template(&fixture.session)
            .unwrap()
            .system_prompt,
        applied.system_prompt
    );

    // Release the worker's context/store references too: reconstruct the host
    // against disk rather than just replacing the request consumer.
    drop(worker);
    fixture = reopen(fixture).await;
    worker = install_compaction(&mut fixture);
    assert_eq!(
        fixture
            .harness
            .request_template(&fixture.session)
            .unwrap()
            .system_prompt,
        applied.system_prompt
    );
    fixture.turn("reopened unchanged").await;
    assert_served_prefix(&fixture, &applied);
    assert_eq!(host_notices(&fixture).len(), notice_count);
    assert_eq!(
        fixture
            .harness
            .request_template(&fixture.session)
            .unwrap()
            .system_prompt,
        applied.system_prompt
    );

    // A second apply reads current files even without an intervening turn-start
    // check. It also folds actual catalog additions/removals into the new prefix.
    std::fs::remove_file(&memory).unwrap();
    std::fs::write(fixture._root.path().join("CLAUDE.md"), "MEMORY-ADDED-V3").unwrap();
    std::fs::remove_dir_all(fixture._root.path().join(".agents/skills/removed")).unwrap();
    skill(&fixture, "added", "CATALOG-ADDED-V3");
    let history_before_second = fixture.store.messages(&fixture.session).unwrap();
    let notice = fixture
        .harness
        .compact_session(&fixture.session, None)
        .await
        .unwrap()
        .unwrap();
    assert!(!notice.contains("Nothing to compact"), "{notice}");
    let second = fixture.harness.request_template(&fixture.session).unwrap();
    assert!(second.system_prompt.contains("MEMORY-ADDED-V3"));
    assert!(!second.system_prompt.contains("MEMORY-APPLIED-V2"));
    assert!(second.system_prompt.contains("CATALOG-ADDED-V3"));
    assert!(!second.system_prompt.contains("CATALOG-REMOVED"));
    assert!(second.system_prompt.contains("CATALOG-APPLIED-V2"));
    assert!(
        second
            .system_prompt
            .contains("BASE-COMPACTION-INSTRUCTIONS")
    );
    assert_eq!(
        serde_json::to_vec(&second.tools).unwrap(),
        serde_json::to_vec(&original.tools).unwrap()
    );
    assert_eq!(
        fixture.store.messages(&fixture.session).unwrap(),
        history_before_second
    );
    assert_eq!(
        fixture
            .store
            .attachment(&fixture.session, "bubblepaw-memory-files-v1")
            .unwrap(),
        original_memory
    );
    assert_eq!(
        fixture
            .store
            .attachment(&fixture.session, "bubblepaw-skills-v1")
            .unwrap(),
        original_skills
    );
    let after_second = fixture.store.storage_stats().unwrap();
    std::fs::write(fixture._root.path().join("CLAUDE.md"), "MEMORY-PENDING-V4").unwrap();
    let notice = fixture
        .harness
        .compact_session(&fixture.session, None)
        .await
        .unwrap()
        .unwrap();
    assert!(notice.contains("Nothing to compact"), "{notice}");
    assert_eq!(
        fixture.store.storage_stats().unwrap().transaction_count,
        after_second.transaction_count
    );
    assert_eq!(
        fixture
            .harness
            .request_template(&fixture.session)
            .unwrap()
            .system_prompt,
        second.system_prompt,
        "a no-op compaction must not rebind changed files"
    );
    fixture.turn("stable second prefix").await;
    assert_served_prefix(&fixture, &second);
    assert_eq!(host_notices(&fixture).len(), notice_count + 1);
    assert!(
        host_notices(&fixture)
            .last()
            .unwrap()
            .content
            .contains("MEMORY-PENDING-V4")
    );
    assert_eq!(
        fixture
            .harness
            .request_template(&fixture.session)
            .unwrap()
            .system_prompt,
        second.system_prompt
    );
    drop(worker);
    fixture = reopen(fixture).await;
    worker = install_compaction(&mut fixture);
    fixture
        .turn("reopen latest snapshot plus later notice")
        .await;
    assert_served_prefix(&fixture, &second);
    assert_eq!(host_notices(&fixture).len(), notice_count + 1);
    assert_eq!(
        fixture
            .harness
            .request_template(&fixture.session)
            .unwrap()
            .system_prompt,
        second.system_prompt,
        "later delivered notices do not replace the applied prefix on reopen"
    );
    fixture.shutdown().await;
    drop(worker);
}

mod classical;
mod custom_base;
