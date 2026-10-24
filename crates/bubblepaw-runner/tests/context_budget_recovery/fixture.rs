use super::*;

impl Fixture {
    pub(super) async fn new(overhead_tokens: usize) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let store = Store::open(root.path().join("archive.sqlite"))?;
        let session = store.create_session("budget recovery")?.id;
        let provider = Arc::new(Scripted::default());
        let policy = Arc::new(
            SummarizingContext::new(
                store.clone(),
                provider.clone(),
                SummaryConfig {
                    threshold_cap: PHYSICAL_CAP,
                    context_window: None,
                    lcm: LcmConfig {
                        output_reserve_tokens: OUTPUT_RESERVE,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                None,
            )?
            .with_auto_summarize(true),
        );
        let models = Arc::new(ScriptedModels::default());
        let worker = EpisodeWorker::new_with_config(
            store.clone(),
            models.clone(),
            policy.clone(),
            EpisodeConfig {
                summary_input: EpisodeSummaryInput::Episode,
                ..Default::default()
            },
        )?;
        policy.set_episode_worker(&worker);

        let overhead = Arc::new(Overhead {
            inner: policy.clone(),
            text: if overhead_tokens == 0 {
                String::new()
            } else {
                format!("{OVERHEAD_MARKER} {}", sized(&policy, overhead_tokens))
            },
            prepared_tokens: AtomicUsize::new(0),
            rejections: AtomicUsize::new(0),
            pressure: Notify::new(),
        });
        let bulky = Arc::new(Bulky {
            output: json!({
                "data": sized(&policy, TOOL_TOKENS),
                "cursor": "recover-here",
                "status": "ready",
            })
            .to_string(),
            invocations: AtomicUsize::new(0),
        });
        assert!(
            bulky.output.len() <= MAX_TOOL_RESULT_BYTES,
            "the tool output must reach the request unreduced"
        );
        let mut tools = ToolRegistry::new();
        tools.register_host(bulky.clone())?;
        let harness = Harness::new(
            store.clone(),
            provider.clone(),
            tools,
            HarnessConfig {
                context: overhead.clone(),
                ..Default::default()
            },
        );

        // Prior applied view: compact covered rounds through the real manual path.
        let covered = sized(&policy, COVERED_ROUND_TOKENS);
        let rounds: Vec<Message> = (0..COVERED_ROUNDS)
            .flat_map(|round| {
                [
                    message(
                        &session,
                        format!("covered-user-{round}"),
                        Role::User,
                        covered.clone(),
                    ),
                    message(
                        &session,
                        format!("covered-assistant-{round}"),
                        Role::Assistant,
                        "Previous work retained.".into(),
                    ),
                ]
            })
            .collect();
        store.append_batch(&rounds)?;
        harness.compact_session(&session, None).await?;
        // Compaction left one permit on `entered`; consume it so the pending
        // gather's entry signal below refers only to the paused catch-up.
        timeout(TIMEOUT, models.entered.notified()).await?;

        // Raw, uncovered tail: the complete round the pressure gather must summarize.
        let tail = vec![
            message(
                &session,
                "tail-user".into(),
                Role::User,
                sized(&policy, TAIL_TOKENS),
            ),
            message(
                &session,
                "tail-assistant".into(),
                Role::Assistant,
                "Previous work retained.".into(),
            ),
        ];
        store.append_batch(&tail)?;
        let covered_before = overhead.history_frontier(&session)?;
        let total = store.messages(&session)?.len();
        Ok(Self {
            _root: root,
            store,
            harness,
            provider,
            policy,
            models,
            worker,
            overhead,
            bulky,
            session,
            covered_before,
            total,
        })
    }

    pub(super) async fn run(&self, input: &str) -> Result<Run> {
        timeout(
            TIMEOUT,
            self.harness
                .prompt(&self.session, input, RunOptions::default()),
        )
        .await?
    }

    pub(super) fn oversized_task(&self) -> String {
        sized(&self.policy, INPUT_BUDGET + 5_000)
    }

    /// Invariants of a recovered run: the overhead is served on both builds, the
    /// first build and the dispatched input fit, one tool execution, one
    /// tool-call request and one final request, and the applied view extended.
    pub(super) fn assert_recovered(&self, run: &Run) -> Result<()> {
        assert_eq!(run.status, "completed", "{:?}", run.error);
        assert!(
            self.overhead.rejections.load(Ordering::SeqCst) >= 1,
            "precondition: the final request must have overflowed on the overhead"
        );
        assert!(
            self.overhead.prepared_tokens.load(Ordering::SeqCst) <= INPUT_BUDGET,
            "precondition: tool growth must pass the ordinary prepare gate"
        );
        assert_eq!(
            self.bulky.invocations.load(Ordering::SeqCst),
            1,
            "the tool runs exactly once"
        );
        let covered_after = self.overhead.history_frontier(&self.session)?;
        assert!(
            covered_after.unwrap_or(0) > self.covered_before.unwrap_or(0),
            "the pressure gather extended the applied view"
        );

        let requests = self.provider.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "one tool-call request and one final request"
        );
        let first = &requests[0];
        for request in &requests {
            assert!(
                serde_json::to_string(&request.messages)?.contains(OVERHEAD_MARKER),
                "the fixed overhead is served on every build"
            );
            assert_eq!(request.system_prompt, first.system_prompt, "frozen prompt");
            assert_eq!(
                serde_json::to_value(&request.tools)?,
                serde_json::to_value(&first.tools)?,
                "frozen tools"
            );
        }
        assert!(
            self.policy.estimated_tokens(first) <= INPUT_BUDGET,
            "the first build fits with the overhead"
        );
        let last = &requests[1];
        assert!(
            self.policy.estimated_tokens(last) <= INPUT_BUDGET,
            "the dispatched input fits the budget"
        );
        assert!(
            serde_json::to_string(&last.messages)?.contains("recover-here"),
            "the latest tool result stays raw in the dispatched input"
        );

        let archive = self.store.messages(&self.session)?;
        let tools: Vec<_> = archive.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tools.len(), 1, "one canonical tool result");
        assert_eq!(
            tools[0].content, self.bulky.output,
            "canonical evidence kept"
        );
        assert!(
            archive
                .iter()
                .any(|m| m.tool_calls.iter().any(|call| call.name == "bulky")),
            "canonical tool call kept"
        );
        Ok(())
    }
}
