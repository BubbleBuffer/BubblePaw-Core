//! A resumable source-only replay stage shared by every chapter-selection arm.
use crate::{
    episode_models::EpisodeModels,
    episode_worker::EpisodeWorker,
    episodes::EpisodeSummaryInput,
    probes::fingerprint,
    provider::ScriptedProvider,
    replay::{ReplayCorpus, ReplayImportBatch, ReplayWorkerGate, normalize_corpus},
    summarization::{SummarizingContext, SummaryConfig},
};
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{
    ContextEpisode, ContextStrategy, MessageOrigin, ModelRequest, Role, Store, is_typed_user,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, sync::Arc, time::SystemTime};

const POLICY: &str = "chapter-bench-production-replay-episodes-v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct FrozenEpisodes {
    pub chat: String,
    pub mode: String,
    pub source_fingerprint: String,
    pub config_fingerprint: String,
    pub session_id: String,
    pub stage_id: String,
    pub complete: bool,
    pub status: String,
    pub messages_total: usize,
    pub messages_imported: usize,
    pub covered_messages: usize,
    pub coverage_through_message_id: Option<String>,
    pub uncovered_tail_messages: usize,
    pub episodes: Vec<ContextEpisode>,
}

#[derive(Deserialize, Serialize)]
struct Stage {
    policy: String,
    mode: String,
    source_fingerprint: String,
    config_fingerprint: String,
    directory: String,
    session_id: String,
}

/// Generate once or resume committed episode coverage. Refresh creates a fresh
/// isolated stage and leaves all previous stage stores available for inspection.
/// The supplied model owns admission, usage persistence and bounded retry policy.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn generate(
    corpus: &ReplayCorpus,
    out: &Path,
    dry_run: bool,
    refresh: bool,
    model_fingerprint: &Value,
    on_stage: &(dyn Fn(&str) + Send + Sync),
    models: Arc<dyn EpisodeModels>,
    mut summary: SummaryConfig,
) -> Result<FrozenEpisodes> {
    ensure!(
        !corpus.chat_id.is_empty()
            && corpus
                .chat_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c)),
        "Chapter benchmark chat ID must be a simple directory name"
    );
    let mut corpus = corpus.clone();
    normalize_corpus(&mut corpus)?;
    ensure!(!corpus.messages.is_empty(), "Recorded chat is empty");
    summary.context_window = summary.context_window.or(corpus.context_window);
    summary.lcm.episodes.summary_input = EpisodeSummaryInput::Episode;
    summary.lcm.episodes.decider_enabled = false;
    summary.lcm.episodes.chapter_rollover_tokens = 0;
    summary.lcm.validate()?;
    let source_fingerprint = {
        let mut generation_source = corpus.clone();
        generation_source.boundary_gold = None;
        generation_source.relates_gold.clear();
        fingerprint(&generation_source)?
    };
    let config_fingerprint = fingerprint(&json!({
        "policy":POLICY,"models":model_fingerprint,"lcm":summary.lcm,
        "context_window":summary.context_window,"threshold_cap":summary.threshold_cap,
        "timeout_millis":summary.timeout.as_millis(),"max_request_bytes":summary.max_request_bytes,
        "verbatim_user_tokens":summary.verbatim_user_tokens,
    }))?;
    let mode = if dry_run { "scripted" } else { "live" };
    let root = out.join("episodes");
    let stages = root.join("stages").join(&corpus.chat_id);
    let pointer = stages.join("current.json");
    let artifact = root.join(format!("{}.json", corpus.chat_id));
    ensure!(
        refresh || !artifact.exists() || pointer.exists(),
        "Frozen episode artifact exists but its stage pointer is missing; restore the stage pointer or explicitly use --refresh-episodes"
    );
    std::fs::create_dir_all(&stages)?;
    let stage: Stage = if pointer.exists() && !refresh {
        read(&pointer)?
    } else {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let directory = format!("stage-{nonce}-{}", std::process::id());
        let store = Store::open_data_dir(stages.join(&directory).join("store"))?;
        let session = store.create_session_with_strategy(
            format!("Chapter benchmark {}", corpus.chat_id),
            ContextStrategy::Lcm,
        )?;
        let stage = Stage {
            policy: POLICY.into(),
            mode: mode.into(),
            source_fingerprint: source_fingerprint.clone(),
            config_fingerprint: config_fingerprint.clone(),
            directory,
            session_id: session.id,
        };
        write(&pointer, &stage)?;
        stage
    };
    ensure!(
        stage.policy == POLICY,
        "Episode stage policy mismatch; use --refresh-episodes"
    );
    ensure!(
        stage.mode == mode,
        "Episode stage mode mismatch; use --refresh-episodes"
    );
    ensure!(
        stage.source_fingerprint == source_fingerprint,
        "Episode stage corpus mismatch; use --refresh-episodes"
    );
    ensure!(
        stage.config_fingerprint == config_fingerprint,
        "Episode stage configuration mismatch; use --refresh-episodes"
    );
    ensure!(
        stage.directory.starts_with("stage-")
            && stage
                .directory
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "Invalid episode stage directory"
    );
    on_stage(&stage.directory);
    let store_dir = stages.join(&stage.directory).join("store");
    ensure!(store_dir.exists(), "Episode stage store is missing");
    let mut store = Store::open_data_dir(store_dir)?;
    let mut messages = corpus.messages.clone();
    for message in &mut messages {
        message.session_id = stage.session_id.clone();
        if message.role == Role::User && message.origin.is_none() {
            message.origin = Some(if is_typed_user(message) {
                MessageOrigin::Typed
            } else {
                MessageOrigin::ChildTask
            });
        }
    }
    let imported = store.messages(&stage.session_id)?;
    ensure!(
        imported.len() <= messages.len(),
        "Episode stage contains extra source messages"
    );
    ensure!(
        fingerprint(&imported)? == fingerprint(&&messages[..imported.len()])?,
        "Episode stage imported history differs from its pinned corpus"
    );
    if !refresh && artifact.exists() {
        let frozen: FrozenEpisodes = read(&artifact)?;
        if frozen.session_id == stage.session_id && frozen.complete {
            ensure!(
                frozen.mode == mode
                    && frozen.source_fingerprint == source_fingerprint
                    && frozen.config_fingerprint == config_fingerprint,
                "Frozen episode artifact fingerprint mismatch"
            );
            ensure!(
                imported.len() == messages.len(),
                "Completed episode artifact has an incomplete store"
            );
            ensure!(
                fingerprint(&frozen.episodes)?
                    == fingerprint(&store.context_episodes(&stage.session_id)?)?,
                "Frozen episode artifact differs from committed episodes"
            );
            return Ok(frozen);
        }
    }
    let policy = summary.lcm.episodes.clone();
    let threshold = summary.threshold();
    let estimator = Arc::new(SummarizingContext::new(
        store.clone(),
        Arc::new(ScriptedProvider::new(
            "Episode generation must not dispatch main requests",
        )),
        summary,
        None,
    )?);
    let worker =
        EpisodeWorker::new_with_config(store.clone(), models, estimator.clone(), policy.clone())?;
    let mut gate = ReplayWorkerGate::with_config(policy);
    let mut history = Vec::new();
    let mut pending = HashSet::new();
    let mut tokens = estimator.estimated_tokens(&ModelRequest {
        system_prompt: corpus.instructions.clone(),
        ..Default::default()
    });
    let mut activated = false;
    let resume_at = imported.len();
    let mut imports = ReplayImportBatch::default();
    for (index, message) in messages.iter().enumerate() {
        if index >= resume_at {
            imports.push(message)?;
            if imports.is_full() {
                imports.flush(&mut store)?;
            }
        }
        history.push(message.clone());
        let message_tokens = estimator.message_tokens(message);
        gate.observe(message, message_tokens)?;
        tokens += message_tokens;
        for call in &message.tool_calls {
            pending.insert(call.id.clone());
        }
        if let Some(call) = &message.tool_call_id {
            pending.remove(call);
        }
        activated |= tokens >= threshold.div_ceil(2);
        // Reconstruct the exact current prefix before consulting its committed
        // coverage; later stored episodes must never enter an earlier prefix.
        if index + 1 < resume_at {
            continue;
        }
        gate.refresh_lcm(&store, &stage.session_id, &history)?;
        let result = async {
            if activated && gate.needs_lcm(&history, &pending)? {
                imports.flush(&mut store)?;
                worker
                    .catch_up_until(&stage.session_id, history.len())
                    .await?;
                gate.refresh_lcm(&store, &stage.session_id, &history)?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            imports.flush(&mut store)?;
            let frozen = snapshot(&corpus, &stage, &store, history.len(), false)?;
            write(&artifact, &frozen)?;
            return Err(
                error.context("Episode generation stopped; committed coverage is resumable")
            );
        }
    }
    imports.flush(&mut store)?;
    let frozen = snapshot(&corpus, &stage, &store, messages.len(), true)?;
    write(&artifact, &frozen)?;
    Ok(frozen)
}

fn snapshot(
    corpus: &ReplayCorpus,
    stage: &Stage,
    store: &Store,
    imported: usize,
    complete: bool,
) -> Result<FrozenEpisodes> {
    let through = store.episode_coverage_token(&stage.session_id)?;
    let covered = through
        .as_ref()
        .map(|id| {
            corpus
                .messages
                .iter()
                .position(|message| &message.id == id)
                .map(|index| index + 1)
                .context("Episode coverage absent from pinned corpus")
        })
        .transpose()?
        .unwrap_or(0);
    Ok(FrozenEpisodes {
        chat: corpus.chat_id.clone(),
        mode: stage.mode.clone(),
        source_fingerprint: stage.source_fingerprint.clone(),
        config_fingerprint: stage.config_fingerprint.clone(),
        session_id: stage.session_id.clone(),
        stage_id: stage.directory.clone(),
        complete,
        status: if complete { "complete" } else { "stopped" }.into(),
        messages_total: corpus.messages.len(),
        messages_imported: imported,
        covered_messages: covered,
        coverage_through_message_id: through,
        uncovered_tail_messages: corpus.messages.len() - covered,
        episodes: store.context_episodes(&stage.session_id)?,
    })
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::episode_models::{
        ChapterInput, ChapterResult, SplitInput, SplitResult, SummaryInput, SummaryResult,
    };
    use async_trait::async_trait;
    use bubblepaw_core::{Message, Usage};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    struct Models {
        fail_second: AtomicBool,
        summarized: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl EpisodeModels for Models {
        async fn decide(&self, _: SplitInput) -> Result<SplitResult> {
            anyhow::bail!("Generation must not invoke a decider")
        }
        async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
            anyhow::bail!("Generation must not close chapters")
        }
        async fn summarize(&self, input: SummaryInput) -> Result<SummaryResult> {
            let mut calls = self.summarized.lock().unwrap();
            if calls.len() == 1 && self.fail_second.swap(false, Ordering::SeqCst) {
                anyhow::bail!("Synthetic admission stop")
            }
            calls.push(input.episode_text);
            Ok(SummaryResult {
                title: "Synthetic episode".into(),
                summary:
                    "# Synthetic episode\nA completed synthetic work step.\n\nOpen: Continue work."
                        .into(),
                model: "scripted".into(),
                usage: Usage::default(),
                reasoning: None,
            })
        }
    }

    fn fixture() -> (ReplayCorpus, SummaryConfig) {
        let messages = (0..7)
            .map(|index| {
                let role = if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                };
                Message {
                    id: format!("r{index}"),
                    session_id: "fixture".into(),
                    origin: (role == Role::User).then_some(MessageOrigin::Typed),
                    role,
                    content: if index % 2 == 0 {
                        "Continue synthetic work.".into()
                    } else {
                        "work ".repeat(120)
                    },
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_state: None,
                    is_error: false,
                    created_at: "2026-10-23T00:00:00Z".into(),
                }
            })
            .collect();
        let corpus = ReplayCorpus {
            chat_id: "synthetic".into(),
            recorded_model: None,
            context_window: None,
            instructions: String::new(),
            messages,
            message_metadata: Value::Null,
            boundary_gold: None,
            relates_gold: vec![],
            provenance: Value::Null,
        };
        let mut summary = SummaryConfig {
            threshold_cap: 100,
            ..Default::default()
        };
        summary.lcm.output_reserve_tokens = 0;
        summary.lcm.episodes.source_target_tokens = 80;
        summary.lcm.episodes.source_max_tokens = 200;
        // Both must be disabled by this stage, even when enabled in host config.
        summary.lcm.episodes.decider_enabled = true;
        summary.lcm.episodes.chapter_rollover_tokens = 1;
        (corpus, summary)
    }

    #[tokio::test]
    async fn interrupted_generation_resumes_commits_and_preserves_short_tail() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (corpus, summary) = fixture();
        let original = fingerprint(&corpus)?;
        let models = Arc::new(Models {
            fail_second: AtomicBool::new(true),
            summarized: Mutex::new(vec![]),
        });
        let signature = json!({"model":"synthetic"});
        let on_stage = |_: &str| {};
        let run = |refresh, dry_run| {
            generate(
                &corpus,
                directory.path(),
                dry_run,
                refresh,
                &signature,
                &on_stage,
                models.clone(),
                summary.clone(),
            )
        };
        assert!(run(false, true).await.is_err());
        let partial: FrozenEpisodes = read(&directory.path().join("episodes/synthetic.json"))?;
        assert!(!partial.complete);
        assert_eq!(partial.episodes.len(), 1);
        let complete = run(false, true).await?;
        assert!(complete.complete);
        assert_eq!(complete.episodes.len(), 3);
        assert_eq!(complete.covered_messages, 6);
        assert_eq!(complete.uncovered_tail_messages, 1);
        assert_eq!(models.summarized.lock().unwrap().len(), 3);
        assert_eq!(partial.episodes[0].id, complete.episodes[0].id);
        assert_eq!(partial.stage_id, complete.stage_id);
        let reused = run(false, true).await?;
        assert_eq!(reused.stage_id, complete.stage_id);
        assert_eq!(models.summarized.lock().unwrap().len(), 3);
        let mut relabeled = corpus.clone();
        relabeled.boundary_gold = Some(json!({"gold":["r4"],"ambiguous":[]}));
        relabeled.relates_gold = vec![json!({"scoring_only":true})];
        let reused = generate(
            &relabeled,
            directory.path(),
            true,
            false,
            &signature,
            &on_stage,
            models.clone(),
            summary.clone(),
        )
        .await?;
        assert_eq!(reused.stage_id, complete.stage_id);
        assert_eq!(models.summarized.lock().unwrap().len(), 3);
        let pointer = directory
            .path()
            .join("episodes/stages/synthetic/current.json");
        let saved_pointer = pointer.with_extension("saved.json");
        std::fs::rename(&pointer, &saved_pointer)?;
        let error = run(false, true)
            .await
            .expect_err("Missing cache pointer must not silently regenerate episodes");
        assert!(error.to_string().contains("stage pointer is missing"));
        assert_eq!(models.summarized.lock().unwrap().len(), 3);
        assert!(!pointer.exists());
        std::fs::rename(saved_pointer, pointer)?;
        assert!(run(false, false).await.is_err());
        let fresh = run(true, true).await?;
        assert_ne!(fresh.stage_id, complete.stage_id);
        assert_eq!(models.summarized.lock().unwrap().len(), 6);
        assert!(
            directory
                .path()
                .join("episodes/stages/synthetic")
                .join(&complete.stage_id)
                .exists()
        );
        assert_eq!(fingerprint(&corpus)?, original);
        Ok(())
    }
}
