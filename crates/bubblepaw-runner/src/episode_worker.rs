//! Background episode coverage. This never prepares or changes the compaction view.
use crate::{
    context_strategies::files_touched,
    episode_models::{
        ChapterInput, ClassicalInput, EpisodeModels, SplitInput, SplitOption, SummaryInput,
    },
    episodes::{EpisodeConfig, EpisodeSummaryInput, text_tokens, token_excerpt},
    history_snapshot::HistorySnapshot,
    summarization::SummarizingContext,
};
use anyhow::{Context, Result};
use bubblepaw_core::{
    ContextCheckpoint, ContextDecision, ContextEpisode, ContextEvent, ContextEventKind,
    ContextStrategy, ContextSubjectChapter, ContextTrigger, DecisionKind, EpisodeBatch, Message,
    MessageOrigin, RequestKind, Role, Store, SubjectChapterStatus, is_typed_user,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, task::AbortHandle};

#[path = "episode_worker_chapters.rs"]
mod chapters;
#[path = "episode_worker_context.rs"]
mod context_provenance;
#[path = "episode_worker_events.rs"]
mod events;

#[derive(Default)]
struct SessionWorker {
    running: bool,
    dirty: bool,
    task: Option<AbortHandle>,
}

#[derive(Clone, PartialEq, Eq)]
struct GatherAttempt {
    previous_coverage: Option<String>,
    first_message_id: String,
    last_message_id: String,
    model_revision: u64,
    main_route: Option<String>,
}
impl GatherAttempt {
    fn suppresses(&self, next: &Self) -> bool {
        self.previous_coverage == next.previous_coverage
            && self.first_message_id == next.first_message_id
            && self.last_message_id == next.last_message_id
            && self.model_revision == next.model_revision
            // An absent or pending capture does not confirm a route change.
            && (next.main_route.is_none() || self.main_route == next.main_route)
    }
}

pub struct EpisodeWorker {
    store: Store,
    models: Arc<dyn EpisodeModels>,
    estimator: Arc<SummarizingContext>,
    config: EpisodeConfig,
    sessions: Mutex<HashMap<String, SessionWorker>>,
    idle: Notify,
    stopping: AtomicBool,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    failed_attempts: Mutex<HashMap<String, GatherAttempt>>,
    /// Diagnostics for a timed-out shutdown: model calls still running, and
    /// completed results per session awaiting an uncommitted derived batch.
    calls_in_flight: AtomicUsize,
    uncommitted_calls: Mutex<HashMap<String, usize>>,
}

impl EpisodeWorker {
    pub fn new(
        store: Store,
        models: Arc<dyn EpisodeModels>,
        estimator: Arc<SummarizingContext>,
    ) -> Arc<Self> {
        Self::new_with_config(store, models, estimator, EpisodeConfig::default())
            .expect("Default episode configuration is valid")
    }
    pub fn new_with_config(
        store: Store,
        models: Arc<dyn EpisodeModels>,
        estimator: Arc<SummarizingContext>,
        config: EpisodeConfig,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        Ok(Arc::new(Self {
            store,
            models,
            estimator,
            config,
            sessions: Mutex::default(),
            idle: Notify::new(),
            stopping: AtomicBool::new(false),
            gates: Mutex::default(),
            failed_attempts: Mutex::default(),
            calls_in_flight: AtomicUsize::new(0),
            uncommitted_calls: Mutex::default(),
        }))
    }
    fn session_exists(&self, session: &str) -> Result<bool> {
        Ok(self.store.active_session_metadata(session)?.is_some())
    }
    pub fn cancel_session(&self, session: &str) {
        self.estimator.forget_episode_capture(session);
        if let Some(state) = self.sessions.lock().unwrap().remove(session)
            && let Some(task) = state.task
        {
            task.abort();
        }
        self.gates.lock().unwrap().remove(session);
        self.failed_attempts.lock().unwrap().remove(session);
        self.uncommitted_calls.lock().unwrap().remove(session);
        self.idle.notify_waiters();
    }
    pub fn reconcile_sessions(&self, live: &[String]) {
        self.estimator.reconcile_episode_captures(live);
        let stale: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .keys()
            .filter(|id| !live.contains(id))
            .cloned()
            .collect();
        for session in stale {
            self.cancel_session(&session);
        }
    }

    /// Gather from complete source rounds independently of serving pressure.
    pub fn trigger(
        self: &Arc<Self>,
        session: &str,
        _context_estimate: usize,
        _threshold: usize,
    ) -> bool {
        if self.stopping.load(Ordering::Acquire) || !self.session_exists(session).unwrap_or(false) {
            return false;
        }
        let mut sessions = self.sessions.lock().unwrap();
        if self.stopping.load(Ordering::Acquire) {
            return false;
        }
        let state = sessions.entry(session.into()).or_default();
        if state.running {
            state.dirty = true;
            return true;
        }
        state.running = true;
        state.dirty = false;
        let worker = self.clone();
        let session = session.to_owned();
        let task = tokio::spawn(async move {
            worker.run(session).await;
        });
        state.task = Some(task.abort_handle());
        true
    }

    /// Wait for the current wake and any triggers coalesced into it.
    pub async fn wait_idle(&self, session: &str) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self
                .sessions
                .lock()
                .unwrap()
                .get(session)
                .is_some_and(|s| s.running)
            {
                return;
            }
            notified.await;
        }
    }

    /// Prevent new wakes and allow in-flight coverage to finish.
    pub async fn shutdown_wait(&self) {
        self.stopping.store(true, Ordering::Release);
        let sessions: Vec<_> = self.sessions.lock().unwrap().keys().cloned().collect();
        for session in sessions {
            self.wait_idle(&session).await;
        }
        self.estimator.clear_episode_captures();
    }

    /// Trace in-flight calls and derived results a bounded shutdown may discard.
    /// Completed physical-attempt usage is preserved separately by its scope.
    pub fn record_shutdown_timeout(&self, drain_timeout: Duration) {
        let aborted = self.calls_in_flight.load(Ordering::Acquire);
        let uncommitted = self.uncommitted_calls.lock().unwrap().values().sum();
        self.models
            .record_shutdown_abort(aborted, uncommitted, drain_timeout);
    }

    async fn counted<T>(
        &self,
        session: &str,
        kind: RequestKind,
        call: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        struct InFlight<'a>(&'a AtomicUsize);
        impl Drop for InFlight<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        self.calls_in_flight.fetch_add(1, Ordering::AcqRel);
        let _in_flight = InFlight(&self.calls_in_flight);
        // A preceding summary may need a decision or chapter call before its
        // derived batch is ready. Retain its billable work before that wait.
        self.store.flush_request_usage(session)?;
        let (result, records) = bubblepaw_core::capture_request_usage_with_store(
            &self.store,
            session,
            None,
            kind,
            call,
        )
        .await;
        for record in records {
            self.store.queue_request_usage(record)?;
        }
        if result.is_err() {
            self.store.flush_request_usage(session)?;
        }
        if result.is_ok() {
            *self
                .uncommitted_calls
                .lock()
                .unwrap()
                .entry(session.into())
                .or_default() += 1;
        }
        result
    }

    /// Completed calls are no longer pending once their batch commits, or once
    /// a failed drain deliberately leaves the source uncovered.
    fn settle_calls(&self, session: &str) {
        self.uncommitted_calls.lock().unwrap().remove(session);
    }

    /// Cancel remaining model calls after the host's bounded graceful shutdown.
    pub fn abort(&self) {
        self.stopping.store(true, Ordering::Release);
        self.estimator.clear_episode_captures();
        for state in self.sessions.lock().unwrap().values_mut() {
            if let Some(task) = state.task.take() {
                task.abort();
            }
            state.running = false;
            state.dirty = false;
        }
        self.idle.notify_waiters();
    }

    async fn run(&self, session: String) {
        loop {
            // Failures leave canonical history uncovered. Pressure catch-up shares
            // this gate and retries before a new serving view can be published.
            let _ = self.drain(&session, None, None, false, None, true).await;
            let mut sessions = self.sessions.lock().unwrap();
            let Some(state) = sessions.get_mut(&session) else {
                return;
            };
            if state.dirty && !self.stopping.load(Ordering::Acquire) {
                state.dirty = false;
                continue;
            }
            state.running = false;
            state.task = None;
            self.idle.notify_waiters();
            return;
        }
    }

    /// Synchronous compaction catch-up shares the background worker's session gate.
    /// Only complete planned episodes ending before the unchanged newest tail are made.
    pub async fn catch_up_until(&self, session: &str, newest_tail_start: usize) -> Result<()> {
        self.drain(session, Some(newest_tail_start), None, false, None, false)
            .await
    }

    /// Deterministic background-equivalent work for hosts such as replay that
    /// need to observe failures. This does not force-refresh an unused Classical
    /// checkpoint; compaction catch-up is the explicit force path.
    pub async fn precompute(&self, session: &str) -> Result<()> {
        self.drain(session, None, None, false, None, false).await
    }

    /// Explicit compaction bypasses activation and episode-size thresholds. Focus
    /// is local to this drain and is never retained by background workers.
    pub async fn compact_until(
        &self,
        session: &str,
        end: usize,
        focus: Option<&str>,
    ) -> Result<()> {
        self.drain(session, Some(end), Some(focus), false, None, false)
            .await
    }

    /// Pressure admission rechecks after each completed episode, so a later
    /// unrelated gather cannot delay a request which already fits. Force permits
    /// one short complete episode only after ordinary cadence has no next range.
    pub async fn gather_pressure_step(
        &self,
        session: &str,
        end: usize,
        force: bool,
        expected_coverage: Option<String>,
    ) -> Result<()> {
        self.models.record_pressure_gather(session, force);
        self.drain(
            session,
            Some(end),
            force.then_some(None),
            true,
            Some(expected_coverage),
            false,
        )
        .await
    }

    async fn drain(
        &self,
        session: &str,
        end_limit: Option<usize>,
        manual: Option<Option<&str>>,
        one_episode: bool,
        expected_coverage: Option<Option<String>>,
        automatic_wake: bool,
    ) -> Result<()> {
        let result = self
            .drain_inner(
                session,
                end_limit,
                manual,
                one_episode,
                expected_coverage,
                automatic_wake,
            )
            .await;
        // Validation failures or disappearing source coverage can skip the
        // derived commit after a successful billable response.
        self.store.flush_request_usage(session)?;
        self.settle_calls(session);
        if let Err(error) = &result {
            self.models.record_drain_failure(session, error);
        }
        result
    }

    async fn drain_inner(
        &self,
        session: &str,
        end_limit: Option<usize>,
        manual: Option<Option<&str>>,
        one_episode: bool,
        expected_coverage: Option<Option<String>>,
        automatic_wake: bool,
    ) -> Result<()> {
        let gate = self
            .gates
            .lock()
            .unwrap()
            .entry(session.into())
            .or_default()
            .clone();
        if !self.session_exists(session)? {
            return Ok(());
        }
        match self.store.session(session)?.context_strategy {
            ContextStrategy::Classical => {
                let _guard = gate.lock().await;
                return self.drain_classical(session, end_limit).await;
            }
            ContextStrategy::Lcm => {}
        }
        loop {
            // Yield the gate between atomic episode commits, allowing pressure
            // admission as soon as sufficient independently valid memory exists.
            let _guard = gate.lock().await;
            if !self.session_exists(session)? {
                return Ok(());
            }
            let token = self.store.episode_coverage_token(session)?;
            if expected_coverage
                .as_ref()
                .is_some_and(|expected| *expected != token)
            {
                return Ok(());
            }
            let mut snapshot = HistorySnapshot::index(&self.store, session)?;
            let coverage_end = token
                .as_ref()
                .map(|id| snapshot.position(id).map(|index| index + 1))
                .transpose()?
                .unwrap_or(0);
            let episodes = self.store.context_episodes(session)?;
            // Canonical headers retain covered call/result ordering and typed
            // request identities without decoding their bodies, including legacy
            // checkpoints whose tool round is still pending.
            if episodes
                .last()
                .is_some_and(|episode| Some(&episode.last_message_id) == token.as_ref())
            {
                snapshot.seed_complete_prefix(coverage_end);
            }
            snapshot.scan(coverage_end, &self.estimator)?;
            let Some(planned) = chapters::next_episode(
                &snapshot,
                coverage_end,
                end_limit,
                manual.is_some(),
                &self.config,
            )?
            else {
                return Ok(());
            };
            let chapters = self.store.context_subject_chapters(session)?;
            let attempt = GatherAttempt {
                previous_coverage: token.clone(),
                first_message_id: planned.message_range.first_message_id.clone(),
                last_message_id: planned.message_range.last_message_id.clone(),
                model_revision: self.models.gather_revision(),
                main_route: self.estimator.episode_fork_route(session),
            };
            if automatic_wake
                && self
                    .failed_attempts
                    .lock()
                    .unwrap()
                    .get(session)
                    .is_some_and(|failed| failed.suppresses(&attempt))
            {
                return Ok(());
            }
            let open = chapters
                .iter()
                .find(|c| c.status == SubjectChapterStatus::Open)
                .cloned();
            let ordinal = episodes.len() + 1;
            let mut decisions = Vec::new();
            let mut closes_previous = false;
            let cap = self.config.summary_target_tokens;
            let input = SummaryInput {
                session_id: session.into(),
                focus: manual.flatten().map(str::to_owned),
                previous_summary: last_valid_summary(&episodes),
                turn_request: planned
                    .origin_request_id
                    .as_ref()
                    .map(|id| {
                        snapshot
                            .message(snapshot.position(id)?)
                            .map(|m| render_task_anchor(&m))
                    })
                    .transpose()?,
                episode_text: render_episode_range(
                    &snapshot,
                    planned.message_range.start,
                    planned.message_range.end,
                    self.config.tool_evidence_tokens,
                )?,
                cap,
            };
            let fork = self.context_fork(
                session,
                &planned.message_range.first_message_id,
                &planned.message_range.last_message_id,
            )?;
            // A failed configured attempt remains suppressed on ordinary wakes
            // for this exact range/route/revision. Explicit admission or manual
            // work may retry, and a successful commit clears the failure key.
            self.failed_attempts
                .lock()
                .unwrap()
                .insert(session.into(), attempt);
            let summary = match fork {
                Some((fork, cold)) => {
                    self.models
                        .record_fork_context(session, &fork.request, &cold);
                    self.counted(
                        session,
                        RequestKind::SummaryFork,
                        self.models.summarize_fork(input, fork),
                    )
                    .await?
                }
                None => {
                    self.counted(
                        session,
                        RequestKind::SummaryFork,
                        self.models.summarize(input),
                    )
                    .await?
                }
            };
            anyhow::ensure!(
                !summary.title.trim().is_empty() && !summary.summary.trim().is_empty(),
                "Episode summarization returned an empty title or summary"
            );
            if let Some(reason) =
                crate::episode_models::summary_output_reason("summary", &summary.summary)
            {
                anyhow::bail!("Episode summarization returned invalid memory: {reason}");
            }
            if !self.session_exists(session)? {
                return Ok(());
            }
            // An episode boundary can change phase without a new typed request.
            if self.config.decider_enabled && open.is_some() {
                let boundary = planned.message_range.first_message_id.clone();
                let result = self
                    .counted(
                        session,
                        RequestKind::Decider,
                        self.models.decide(SplitInput {
                            session_id: session.into(),
                            open_chapter: open_representation(
                                open.as_ref(),
                                &episodes,
                                self.config.decision_context_tokens,
                            ),
                            options: vec![SplitOption {
                                message_id: boundary.clone(),
                                user: format!(
                                    "EPISODE SUMMARY (not typed user input):\n{}",
                                    summary.summary
                                ),
                                agent_before: String::new(),
                            }],
                        }),
                    )
                    .await?;
                anyhow::ensure!(
                    result.choice.as_ref().is_none_or(|id| id == &boundary),
                    "Episode phase decision selected an unoffered boundary"
                );
                closes_previous = result.choice.is_some();
                decisions.push(ContextDecision {
                    id: new_id(),
                    session_id: session.into(),
                    kind: DecisionKind::Split,
                    episode_ordinal: ordinal,
                    option_message_ids: vec![boundary],
                    probabilities: result.probabilities,
                    choice: result.choice,
                    model: result.model,
                    usage: Some(result.usage),
                    reasoning: result.reasoning,
                    created_at: now(),
                });
            }
            if !self.session_exists(session)? {
                return Ok(());
            }
            let previous_closed = if closes_previous {
                Some(
                    self.close_chapter(
                        session,
                        open.clone().unwrap(),
                        &episodes,
                        &chapters,
                        &snapshot,
                    )
                    .await?,
                )
            } else {
                None
            };
            let mut chapter = match open.filter(|_| !closes_previous) {
                Some(chapter) => chapter,
                None => chapters::new_open_chapter(session, chapters.len() + 1, ordinal),
            };
            chapter.last_episode_ordinal = ordinal;
            let episode = ContextEpisode {
                id: new_id(),
                session_id: session.into(),
                ordinal,
                first_message_id: planned.message_range.first_message_id,
                last_message_id: planned.message_range.last_message_id,
                source_tokens: planned.source_tokens,
                cap,
                origin_request_id: planned.origin_request_id,
                title: summary.title,
                summary: summary.summary,
                model: summary.model,
                usage: Some(summary.usage),
                reasoning: summary.reasoning,
                created_at: now(),
                chapter_id: chapter.id.clone(),
            };
            let chapter_summary_tokens = episodes
                .iter()
                .filter(|e| e.chapter_id == chapter.id)
                .map(|e| text_tokens(&e.summary))
                .sum::<usize>()
                .saturating_add(text_tokens(&episode.summary));
            let close_after_episode = if previous_closed.is_none()
                && self.config.chapter_rollover_tokens > 0
                && chapter_summary_tokens >= self.config.chapter_rollover_tokens
            {
                let mut including = episodes.clone();
                including.push(episode.clone());
                if !self.session_exists(session)? {
                    return Ok(());
                }
                let mut closed = self
                    .close_chapter(session, chapter.clone(), &including, &chapters, &snapshot)
                    .await?;
                closed.description = Some(format!(
                    "[Continues same subject; volume rollover] {}",
                    closed.description.as_deref().unwrap_or_default()
                ));
                Some(closed)
            } else {
                None
            };
            let batch = EpisodeBatch {
                episode,
                decisions,
                open_chapter: chapter,
                closed_chapter: previous_closed,
                close_after_episode,
                expected_previous_coverage: token.clone(),
            };
            let trigger = match (one_episode, manual.is_some()) {
                (true, true) => ContextTrigger::Emergency,
                (true, false) => ContextTrigger::Pressure,
                (false, true) => ContextTrigger::Manual,
                (false, false) => ContextTrigger::SourceSize,
            };
            let split_before_seq =
                closes_previous.then(|| snapshot.seq(planned.message_range.start));
            let events =
                events::gather_events(&batch, &episodes, trigger, split_before_seq, &self.config)?;
            if !self.session_exists(session)? {
                return Ok(());
            }
            if let Err(error) = self.store.save_episode_batch_with_events(&batch, &events) {
                if self.store.episode_coverage_token(session)? != token {
                    continue;
                }
                return Err(error);
            }
            self.settle_calls(session);
            self.failed_attempts.lock().unwrap().remove(session);
            if one_episode {
                return Ok(());
            }
        }
    }

    async fn drain_classical(&self, session: &str, end_limit: Option<usize>) -> Result<()> {
        let previous = self.store.context_checkpoint(session)?;
        if end_limit.is_none()
            && let Some(checkpoint) = &previous
        {
            let covered = self
                .store
                .message_seq(session, &checkpoint.through_message_id)?;
            let applied = self
                .store
                .context_view(session)?
                .and_then(|view| view.frontier);
            if applied.is_none_or(|frontier| covered > frontier) {
                // Keep one pending precomputation until it is applied. Rewriting
                // it on every committed message adds model calls and drift while
                // the provider still sees the same frozen context.
                return Ok(());
            }
        }
        let mut snapshot = HistorySnapshot::index(&self.store, session)?;
        let (imported_frontier, imported_prefix) = self.store.imported_history_seed(session)?;
        let start = previous
            .as_ref()
            .map(|checkpoint| {
                snapshot
                    .position(&checkpoint.through_message_id)
                    .map(|index| index + 1)
            })
            .transpose()?
            .or(imported_frontier.map(|seq| {
                snapshot
                    .metadata
                    .iter()
                    .enumerate()
                    .find(|(index, _)| snapshot.seq(*index) == seq)
                    .map(|(index, _)| index + 1)
                    .unwrap_or(0)
            }))
            .unwrap_or(0);
        // Classical checkpoints are validated complete boundaries. The older
        // bodies supply neither pending calls nor new summary source text.
        snapshot.scan(start, &self.estimator)?;
        let end = crate::context_strategies::complete_prefix_end(
            &snapshot.metadata,
            end_limit
                .unwrap_or(snapshot.metadata.len())
                .min(snapshot.tail_start(32_000)),
        );
        if end <= start {
            return Ok(());
        }
        let mut span_text = String::new();
        let mut latest_user_request = None;
        let mut touched = previous
            .as_ref()
            .map(|checkpoint| checkpoint.files_touched.clone())
            .unwrap_or_default();
        snapshot.walk(start, end, |_, message| {
            if is_typed_user(&message) {
                latest_user_request = Some(message.content.clone());
            }
            if !span_text.is_empty() {
                span_text.push_str("\n\n");
            }
            span_text.push_str(&format!(
                "Date: {}\n{}",
                message.created_at,
                render_episode(std::slice::from_ref(&message))
            ));
            touched = files_touched(&touched, std::slice::from_ref(&message));
            Ok(())
        })?;
        let result = self
            .counted(
                session,
                RequestKind::SummaryFork,
                self.models.summarize_classical(ClassicalInput {
                    session_id: session.into(),
                    previous_summary: previous
                        .as_ref()
                        .map(|checkpoint| checkpoint.summary.clone())
                        .or_else(|| {
                            (!imported_prefix.is_empty()).then(|| {
                                imported_prefix
                                    .iter()
                                    .map(|message| message.content.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                        }),
                    latest_user_request,
                    span_text,
                }),
            )
            .await?;
        anyhow::ensure!(
            result.summary.len() <= 64 * 1024,
            "Classical model output exceeds 64 KiB"
        );
        let checkpoint = ContextCheckpoint {
            id: new_id(),
            session_id: session.into(),
            through_message_id: snapshot.metadata[end - 1].id.clone(),
            summary: result.summary,
            model: result.model,
            created_at: now(),
            usage: Some(result.usage),
            files_touched: touched,
        };
        if !self.session_exists(session)? {
            return Ok(());
        }
        self.store.save_context_checkpoint(&checkpoint)?;
        self.settle_calls(session);
        Ok(())
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn open_representation(
    open: Option<&ContextSubjectChapter>,
    episodes: &[ContextEpisode],
    budget: usize,
) -> String {
    let Some(open) = open else {
        return String::new();
    };
    let mut members: Vec<_> = episodes
        .iter()
        .filter(|episode| episode.chapter_id == open.id)
        .collect();
    let header = "OPEN CHAPTER accumulated state. Later corrections supersede earlier state.\n";
    let label = |episode: &ContextEpisode| {
        format!(
            "\nEPISODE {} [source {}..{}]\n",
            episode.ordinal, episode.first_message_id, episode.last_message_id
        )
    };
    // Keep complete reference headers. With a tiny budget, omit whole middle
    // episodes rather than cutting an ID; retain the first and latest where possible.
    let render = |members: &[&ContextEpisode], share| {
        let mut state = header.to_owned();
        for episode in members {
            state.push_str(&label(episode));
            state.push_str(&token_excerpt(&episode.summary, share));
        }
        state
    };
    while !members.is_empty() && text_tokens(&render(&members, 0)) > budget {
        let index = if members.len() > 2 {
            members.len() / 2
        } else {
            0
        };
        members.remove(index);
    }
    if members.is_empty() {
        return token_excerpt(header, budget);
    }
    let mut share = budget.saturating_sub(text_tokens(&render(&members, 0))) / members.len();
    loop {
        let state = render(&members, share);
        if text_tokens(&state) <= budget {
            return state;
        }
        // Token segmentation across joins can add a token. Only shorten bodies.
        share = share.saturating_sub(1);
    }
}

/// An episode stored before today's validation may hold invalid memory. It is
/// never shown as PREVIOUS EPISODE; the last valid summary, or none, is.
fn last_valid_summary(episodes: &[ContextEpisode]) -> Option<String> {
    episodes
        .iter()
        .rev()
        .find(|episode| {
            crate::episode_models::summary_output_reason("summary", &episode.summary).is_none()
        })
        .map(|episode| episode.summary.clone())
}

pub(crate) fn render_task_anchor(message: &Message) -> String {
    let label = if is_typed_user(message) {
        "Typed request"
    } else if message.origin == Some(MessageOrigin::ChildTask) {
        "ChildTask instruction (not a human statement)"
    } else {
        "Generated context (not a typed request)"
    };
    format!("[source {}: {label}]\n{}", message.id, message.content)
}

pub(crate) fn render_episode_range(
    snapshot: &HistorySnapshot,
    start: usize,
    end: usize,
    budget: usize,
) -> Result<String> {
    let mut messages = Vec::new();
    snapshot.walk(start, end, |_, message| {
        messages.push(message);
        Ok(())
    })?;
    Ok(render_episode_with_budget(&messages, budget))
}
fn render_episode(messages: &[Message]) -> String {
    render_episode_with_budget(messages, EpisodeConfig::default().tool_evidence_tokens)
}
fn render_episode_with_budget(messages: &[Message], budget: usize) -> String {
    let payloads: usize = messages
        .iter()
        .map(|m| usize::from(m.role == Role::Tool) + m.tool_calls.len())
        .sum();
    let mut left = budget;
    let mut remaining = payloads;
    let mut allocation = || {
        if remaining == 0 {
            return 0;
        }
        let share = left / remaining;
        left -= share;
        remaining -= 1;
        share
    };
    messages
        .iter()
        .map(|m| {
            let content = if m.role == Role::Tool {
                let status = if tool_result_failed(m) { "error" } else { "ok" };
                // Zero opts out of lossy evidence projection. Copy only the
                // canonical message delivered to the main model; never read a
                // referenced spill file or reconstruct a larger original body.
                let evidence = if budget == 0 {
                    m.content.clone()
                } else {
                    tool_evidence(m, allocation())
                };
                format!(
                    "[tool output {}, {} chars, {status}]\n{evidence}",
                    m.tool_call_id.as_deref().unwrap_or(&m.id),
                    m.content.chars().count()
                )
            } else if m.role == Role::User {
                render_task_anchor(m)
            } else {
                m.content.clone()
            };
            // Visible content only: provider_state and raw reasoning are never included.
            let mut text = format!("[{} {:?}]\n{content}", m.id, m.role);
            for call in &m.tool_calls {
                let arguments = call.arguments.to_string();
                let excerpt = if budget == 0 {
                    arguments.clone()
                } else {
                    token_excerpt(&arguments, allocation())
                };
                text.push_str(&format!(
                    "\nTool call {} ({}) arguments ({} tokens):\n{}",
                    call.id,
                    call.name,
                    text_tokens(&arguments),
                    excerpt
                ));
            }
            if let Some(call) = &m.tool_call_id {
                text.push_str(&format!("\nTool call ID: {call}"));
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
fn tool_evidence(message: &Message, budget: usize) -> String {
    if text_tokens(&message.content) <= budget {
        return message.content.clone();
    }
    let result = message
        .content
        .split("\n[---]\n")
        .next()
        .unwrap_or_default();
    let mut facts = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(result) {
        structured_tool_facts(&value, "$", &mut facts);
    }
    let facts = facts.join("\n");
    let structured = token_excerpt(&facts, text_tokens(&facts).min(budget / 2));
    let excerpt = token_excerpt(
        &message.content,
        budget.saturating_sub(text_tokens(&structured) + usize::from(!structured.is_empty())),
    );
    let evidence = if structured.is_empty() {
        excerpt
    } else {
        format!("{structured}\n{excerpt}")
    };
    token_excerpt(&evidence, budget)
}
fn structured_tool_facts(value: &serde_json::Value, path: &str, facts: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                let path = format!("{path}.{key}");
                if matches!(
                    key.as_str(),
                    "status"
                        | "exitCode"
                        | "exit_code"
                        | "is_error"
                        | "error"
                        | "last_error"
                        | "path"
                        | "handle"
                        | "truncated"
                        | "source"
                        | "call_id"
                        | "stderr"
                        | "diagnostics"
                        | "failures"
                ) && !value.is_null()
                {
                    facts.push(format!("{path}={value}"));
                } else if value.is_object() || value.is_array() {
                    structured_tool_facts(value, &path, facts);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                structured_tool_facts(value, &format!("{path}[{index}]"), facts);
            }
        }
        _ => {}
    }
}

fn tool_result_failed(message: &Message) -> bool {
    if message.is_error {
        return true;
    }
    // JS state notices follow the serialized return. They are not result data.
    let result = message.content.split("\n[---]\n").next().unwrap();
    serde_json::from_str(result)
        .map(|value| result_record_failed(&value))
        .unwrap_or(false)
}

fn result_record_failed(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(records) => records.iter().any(result_record_failed),
        serde_json::Value::Object(record) => {
            // These fields are emitted by process replies, child replies, execution
            // receipts and environment inspection. Never infer errors from stdout,
            // stderr, diagnostics, preview strings or arbitrary prose.
            record.get("is_error").is_some_and(|flag| flag == true)
                || ["exitCode", "exit_code"].iter().any(|key| {
                    record
                        .get(*key)
                        .and_then(serde_json::Value::as_f64)
                        .is_some_and(|code| code != 0.0)
                })
                || ["error", "last_error"].iter().any(|key| {
                    record.get(*key).is_some_and(|error| match error {
                        serde_json::Value::Null => false,
                        serde_json::Value::Bool(flag) => *flag,
                        serde_json::Value::String(text) => !text.is_empty(),
                        _ => true,
                    })
                })
                || record
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|status| matches!(status, "failed" | "timed_out" | "cancelled"))
                || record.values().any(result_record_failed)
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "episode_worker_input_tests.rs"]
mod summary_input_tests;
