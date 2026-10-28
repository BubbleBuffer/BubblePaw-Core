#[path = "support/episode_fixture.rs"]
mod episode_fixture;
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, Message, ModelRequest, ModelResponse, Provider,
    Role, RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition,
    ToolRegistry, ToolResult, Usage,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    episode_worker::EpisodeWorker,
    summarization::{SUMMARY_PROMPT, SummarizingContext, SummaryConfig},
};
use episode_fixture::new_episode_worker;
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct OfflineActions {
    plans: Mutex<VecDeque<Vec<Value>>>,
    acted_on: Mutex<HashSet<String>>,
    requests: Mutex<Vec<ModelRequest>>,
}
impl OfflineActions {
    fn schedule(&self, cells: &[&str]) {
        self.plans
            .lock()
            .unwrap()
            .push_back(cells.iter().map(|code| json!({"code":code})).collect());
    }
    fn schedule_with_limit(&self, cells: &[&str], max_output_tokens: usize) {
        self.plans.lock().unwrap().push_back(
            cells
                .iter()
                .map(|code| json!({"code":code,"maxOutputTokens":max_output_tokens}))
                .collect(),
        );
    }
    fn summary_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.system_prompt == SUMMARY_PROMPT)
            .count()
    }
}
#[async_trait]
impl Provider for OfflineActions {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let summary = request.system_prompt == SUMMARY_PROMPT;
        self.requests.lock().unwrap().push(request.clone());
        if summary {
            return Ok(ModelResponse { text: "# Completed JS action\nThe bounded JS action finished; retain its findings and continue the literal latest user request.".into(), ..Default::default() });
        }
        let user = request
            .messages
            .iter()
            .rfind(|m| m.role == Role::User)
            .unwrap();
        let first_request = self.acted_on.lock().unwrap().insert(user.id.clone());
        if first_request && let Some(cells) = self.plans.lock().unwrap().pop_front() {
            return Ok(ModelResponse {
                tool_calls: cells
                    .into_iter()
                    .map(|arguments| ToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: "js".into(),
                        arguments,
                    })
                    .collect(),
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "Continued after the action.".into(),
            ..Default::default()
        })
    }
}

struct BulkyNative;
#[async_trait]
impl Tool for BulkyNative {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "bulky".into(),
            description: "Synthetic oversized structured read".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        Ok(ToolResult::text(
            json!({"data":"large result ".repeat(10000),"cursor":"recover-here","status":"ready"})
                .to_string(),
        ))
    }
}

struct FakeEpisodes;
#[async_trait]
impl EpisodeModels for FakeEpisodes {
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len() + 1];
        *probabilities.last_mut().unwrap() = 1.0;
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "fake".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn summarize(&self, _: SummaryInput) -> Result<SummaryResult> {
        Ok(SummaryResult {
            title: "Completed action".into(),
            summary: "Evidence retained; continue.".into(),
            model: "fake".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        Ok(ChapterResult {
            description: "subject".into(),
            summary: "Retained subject".into(),
            related_ids: vec![],
            related_evidence: vec![],
            decisions: None,
            sections: None,
            model: "fake".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    harness: Harness,
    provider: Arc<OfflineActions>,
    policy: Arc<SummarizingContext>,
    session: String,
    _worker: Arc<EpisodeWorker>,
}

fn estimated_output_tokens(text: &str) -> Result<usize> {
    Ok(
        tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?
            .encode_ordinary(text)
            .len(),
    )
}

fn spilled_response(fixture: &Fixture, receipt: &Value) -> Result<String> {
    let path = receipt["fullOutput"]["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Truncation receipt has no spill path"))?;
    let path = Path::new(path);
    assert!(path.is_absolute());
    let directory = fixture
        .store
        .data_dir()
        .unwrap()
        .join("spill")
        .join(&fixture.session);
    assert_eq!(path.parent(), Some(directory.as_path()));
    assert_eq!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("txt")
    );
    let full = std::fs::read_to_string(path)?;
    assert_eq!(
        receipt["fullOutput"]["bytes"].as_u64(),
        Some(full.len() as u64)
    );
    Ok(full)
}
impl Fixture {
    fn new(cap: usize, window: Option<usize>) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let store = Store::open(root.path().join("archive.sqlite"))?;
        let session = store.create_session("adversarial growth")?.id;
        let artifacts = Arc::new(ArtifactStore::new(
            root.path(),
            store.data_dir().unwrap().join("artifacts"),
        )?);
        let manager = Arc::new(CodeMode::new(root.path().to_owned(), artifacts)?);
        let mut tools = ToolRegistry::new();
        tools.register_host(Arc::new(BulkyNative))?;
        manager.register(&mut tools)?;
        let provider = Arc::new(OfflineActions::default());
        let policy = Arc::new(SummarizingContext::new(
            store.clone(),
            provider.clone(),
            SummaryConfig {
                threshold_cap: cap,
                lcm: bubblepaw_runner::summarization::LcmConfig {
                    // This fixture's caps are synthetic input-only budgets.
                    output_reserve_tokens: 0,
                    ..Default::default()
                },
                context_window: window,
                ..Default::default()
            },
            None,
        )?);
        let worker = new_episode_worker(store.clone(), Arc::new(FakeEpisodes), policy.clone());
        policy.set_episode_worker(&worker);
        let harness = Harness::new(
            store.clone(),
            provider.clone(),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                context: policy.clone(),
                ..Default::default()
            },
        );
        Ok(Self {
            _root: root,
            store,
            harness,
            provider,
            policy,
            session,
            _worker: worker,
        })
    }
    async fn prompt(&self, input: &str) -> Result<bubblepaw_core::Run> {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.harness
                .prompt(&self.session, input, RunOptions::default()),
        )
        .await?
    }
    fn seed(&self, text: String) -> Result<Vec<Message>> {
        let chars: Vec<_> = text.chars().collect();
        let turns = chars.chunks(40000).flat_map(|chunk| {
            [
                (Role::User, chunk.iter().collect::<String>()),
                (Role::Assistant, "Previous work retained.".into()),
            ]
        });
        let messages: Vec<_> = turns
            .map(|(role, content)| Message {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: self.session.clone(),
                role,
                content,
                tool_calls: vec![],
                tool_call_id: None,
                origin: None,
                provider_state: None,
                is_error: false,
                created_at: "2026-10-01T00:00:00Z".into(),
            })
            .collect();
        self.store.append_batch(&messages)?;
        self.store.messages(&self.session)
    }

    fn assert_summarized_continuation(
        &self,
        original: &[Message],
        latest: &str,
        outputs: &[String],
        threshold: usize,
        summarize_latest_group: bool,
    ) -> Result<()> {
        let archive = self.store.messages(&self.session)?;
        assert_eq!(
            serde_json::to_value(&archive[..original.len()])?,
            serde_json::to_value(original)?
        );
        let tools: Vec<_> = archive.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tools.len(), outputs.len(), "Every action ran exactly once");
        for (tool, output) in tools.iter().zip(outputs) {
            assert!(!tool.is_error);
            assert_eq!(tool.content, serde_json::to_string(output)?);
        }
        let groups: Vec<_> = archive
            .iter()
            .filter(|m| !m.tool_calls.is_empty())
            .collect();
        assert_eq!(groups.len(), 1, "One assistant action group was executed");
        assert_eq!(groups[0].tool_calls.len(), outputs.len());
        let call_ids: HashSet<_> = groups[0].tool_calls.iter().map(|call| &call.id).collect();
        assert_eq!(
            call_ids,
            tools
                .iter()
                .map(|tool| tool.tool_call_id.as_ref().unwrap())
                .collect()
        );
        let publications = self.store.outputs(&self.session, 32)?;
        for index in 0..outputs.len() {
            let marker = format!("growth-action-{index}");
            assert_eq!(
                publications
                    .iter()
                    .filter(
                        |output| output.output["type"] == "text" && output.output["text"] == marker
                    )
                    .count(),
                1,
                "Completed publication {marker} must not be replayed"
            );
        }
        let view = self.store.context_view(&self.session)?.unwrap();
        assert!(view.masked_tool_output_ids.is_empty());
        assert!(view.clipped_tool_call_ids.is_empty());
        let frontier = view.frontier.expect("Actual summary coverage was applied");
        let group_start = self.store.message_seq(&self.session, &groups[0].id)?;
        let group_end = self
            .store
            .message_seq(&self.session, &tools.last().unwrap().id)?;
        assert!(
            frontier < group_start || frontier >= group_end,
            "The view must not split a call/result group"
        );
        if summarize_latest_group {
            assert!(
                frontier >= group_end,
                "The complete oversized group is summarized"
            );
        }
        let requests = self.provider.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "Actions followed by one natural continuation"
        );
        let resumed = requests.last().unwrap();
        assert!(self.policy.estimated_tokens(resumed) < threshold);
        let anchor = archive
            .iter()
            .find(|m| bubblepaw_core::is_typed_user(m) && m.content == latest)
            .unwrap();
        assert_eq!(
            serde_json::to_value(resumed.messages.iter().find(|m| m.id == anchor.id).unwrap())?,
            serde_json::to_value(anchor)?,
            "The actual latest typed instruction stays byte-exact"
        );
        let episodes = self.store.context_episodes(&self.session)?;
        assert!(
            episodes.iter().any(|episode| {
                episode.summary == "Evidence retained; continue."
                    && self
                        .store
                        .message_seq(&self.session, &episode.last_message_id)
                        .unwrap()
                        <= frontier
                    && resumed
                        .messages
                        .iter()
                        .any(|m| m.content.contains(&episode.summary))
            }),
            "The continuation serves a saved episode summary, not just a smaller masked view"
        );
        for message in groups.iter().copied().chain(tools.iter().copied()) {
            if self.store.message_seq(&self.session, &message.id)? <= frontier {
                assert!(resumed.messages.iter().all(|m| m.id != message.id));
            } else {
                assert_eq!(
                    serde_json::to_value(
                        resumed
                            .messages
                            .iter()
                            .find(|m| m.id == message.id)
                            .unwrap()
                    )?,
                    serde_json::to_value(message)?,
                    "Uncovered call/result messages remain exact"
                );
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn small_threshold_summarizes_ready_coverage_without_discarding_completed_effects()
-> Result<()> {
    let f = Fixture::new(8000, Some(40000))?;
    let original = f.seed("old evidence ".repeat(1700))?;
    f.provider
        .schedule(&["text('growth-action-0'); return 'piece '.repeat(6000);"]);
    let latest = "Inspect the large report, preserve my exact instruction, and continue.";
    let run = f.prompt(latest).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    f.assert_summarized_continuation(&original, latest, &["piece ".repeat(6000)], 8000, false)?;
    Ok(())
}

#[tokio::test]
async fn publications_stay_outside_context_and_large_return_spills_without_losing_state()
-> Result<()> {
    let f = Fixture::new(24000, Some(40000))?;
    f.provider
        .schedule(&["for(let i=0;i<3;i++) text('publication '.repeat(4500)); return {ok:true};"]);
    assert_eq!(
        f.prompt("Publish the three reports.").await?.status,
        "completed"
    );
    assert_eq!(
        f.store
            .outputs(&f.session, 20)?
            .iter()
            .filter(|o| o.output["type"] == "text")
            .count(),
        3
    );
    assert_eq!(
        f.store
            .outputs(&f.session, 20)?
            .iter()
            .filter_map(|o| o.output["text"].as_str().map(str::len))
            .sum::<usize>(),
        162000
    );
    let messages = f.store.messages(&f.session)?;
    assert!(
        messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .all(|m| m.content.len() < 1024)
    );
    assert_eq!(f.provider.summary_count(), 0);
    f.provider
        .schedule(&["state.kept=19; state.fullBody='x '.repeat(35000); return {body:state.fullBody,kept:state.kept};"]);
    assert_eq!(
        f.prompt("Return an oversized value.").await?.status,
        "completed"
    );
    let archive = f.store.messages(&f.session)?;
    let large = archive.iter().rev().find(|m| m.role == Role::Tool).unwrap();
    assert!(!large.is_error);
    let receipt: Value = serde_json::from_str(&large.content)?;
    assert_eq!(receipt["truncated"], true);
    assert!(estimated_output_tokens(&large.content)? <= 10000);
    let full = spilled_response(&f, &receipt)?;
    let value: Value = serde_json::from_str(full.split("\n[---]\n").next().unwrap())?;
    assert_eq!(value["body"].as_str().unwrap().len(), 70000);
    assert_eq!(value["kept"], 19);
    f.provider
        .schedule(&["return {recovered:true,kept:state.kept,fullLength:state.fullBody.length};"]);
    assert_eq!(
        f.prompt("Use a small result and continue.").await?.status,
        "completed"
    );
    let archive = f.store.messages(&f.session)?;
    let recovery = archive.iter().rev().find(|m| m.role == Role::Tool).unwrap();
    assert!(!recovery.is_error);
    assert!(recovery.content.contains("recovered"));
    assert_eq!(
        serde_json::from_str::<Value>(recovery.content.split("\n[---]\n").next().unwrap())?["kept"],
        19
    );
    assert_eq!(
        serde_json::from_str::<Value>(recovery.content.split("\n[---]\n").next().unwrap())?["fullLength"],
        70000
    );
    Ok(())
}

#[tokio::test]
async fn oversized_nested_native_json_spills_with_bounded_preview_and_cursor() -> Result<()> {
    let f = Fixture::new(24000, Some(40000))?;
    f.provider.schedule(&["return await tools.bulky({});"]);
    assert_eq!(
        f.prompt("Read the oversized structured result.")
            .await?
            .status,
        "completed"
    );
    let archive = f.store.messages(&f.session)?;
    let tool = archive.iter().find(|m| m.role == Role::Tool).unwrap();
    assert!(!tool.is_error);
    let receipt: Value = serde_json::from_str(&tool.content)?;
    assert_eq!(receipt["truncated"], true);
    assert_eq!(receipt["previewFormat"], "text");
    assert_eq!(receipt["cursor"], "recover-here");
    assert_eq!(receipt["status"], "ready");
    assert!(
        receipt["preview"]
            .as_str()
            .unwrap()
            .contains("output preview truncated")
    );
    assert!(tool.content.len() <= 60 * 1024);
    assert!(estimated_output_tokens(&tool.content)? <= 10000);
    let full = spilled_response(&f, &receipt)?;
    assert!(full.len() > 64 * 1024);
    assert_eq!(
        serde_json::from_str::<Value>(&full)?,
        json!({"data":"large result ".repeat(10000),"cursor":"recover-here","status":"ready"})
    );
    assert_eq!(f.provider.summary_count(), 0);
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "The native action runs once, followed by a bounded continuation"
    );
    assert!(f.policy.estimated_tokens(requests.last().unwrap()) < 24000);
    Ok(())
}

#[tokio::test]
async fn per_cell_publication_limit_fails_small_preserving_already_published_outputs() -> Result<()>
{
    let f = Fixture::new(24000, Some(40000))?;
    f.provider
        .schedule(&["for(let i=0;i<4;i++) text('x'.repeat(60000));"]);
    let run = f.prompt("Publish until the per-cell output limit.").await?;
    assert_eq!(run.status, "completed");
    let outputs = f.store.outputs(&f.session, 20)?;
    assert_eq!(
        outputs
            .iter()
            .filter(|o| o.output["type"] == "text")
            .count(),
        3
    );
    assert_eq!(
        outputs
            .iter()
            .filter_map(|o| o.output["text"].as_str().map(str::len))
            .sum::<usize>(),
        180000
    );
    let archive = f.store.messages(&f.session)?;
    let tool = archive.iter().find(|m| m.role == Role::Tool).unwrap();
    assert!(tool.is_error);
    assert!(tool.content.contains("Cell output limit reached"));
    assert!(tool.content.len() < 1024);
    assert_eq!(f.provider.summary_count(), 0);
    Ok(())
}

#[tokio::test]
async fn default_dense_return_stays_bounded_and_dispatches_without_exceeding_small_window()
-> Result<()> {
    let f = Fixture::new(24000, Some(40000))?;
    let original = f.seed("previous ".repeat(10000))?;
    f.provider.schedule(&["return 'x '.repeat(30000);"]);
    let run = f
        .prompt("Read the dense result but retain this latest user instruction.")
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(f.provider.summary_count(), 0);
    assert!(f.store.context_chapters(&f.session)?.is_empty());
    assert!(f.store.context_checkpoint(&f.session)?.is_none());
    let archive = f.store.messages(&f.session)?;
    assert_eq!(
        serde_json::to_value(&archive[..original.len()])?,
        serde_json::to_value(original)?
    );
    let tool = archive.iter().find(|m| m.role == Role::Tool).unwrap();
    assert!(!tool.is_error);
    let receipt: Value = serde_json::from_str(&tool.content)?;
    assert_eq!(receipt["truncated"], true);
    let result_tokens = estimated_output_tokens(&tool.content)?;
    assert!(result_tokens <= 10000);
    let full = spilled_response(&f, &receipt)?;
    assert_eq!(full.len(), 60002);
    assert_eq!(serde_json::from_str::<String>(&full)?, "x ".repeat(30000));
    let regular = f
        .provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.system_prompt != SUMMARY_PROMPT)
        .count();
    assert_eq!(regular, 2, "A bounded continuation is dispatched");
    eprintln!(
        "dense single JS result after default budget: {} bytes, {result_tokens} tokenizer tokens; original60002bytes retained in spill",
        tool.content.len()
    );
    Ok(())
}

#[tokio::test]
async fn several_legal_returns_in_one_model_round_are_summarized_as_a_complete_group() -> Result<()>
{
    let f = Fixture::new(24000, Some(40000))?;
    let original = f.seed("previous ".repeat(10000))?;
    let cells: Vec<_> = (0..4)
        .map(|index| format!("text('growth-action-{index}'); return 'piece '.repeat(8000);"))
        .collect();
    f.provider
        .schedule(&cells.iter().map(String::as_str).collect::<Vec<_>>());
    let latest = "Read all four results and retain this literal current objective.";
    let run = f.prompt(latest).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let archive = f.store.messages(&f.session)?;
    assert_eq!(
        serde_json::to_value(&archive[..original.len()])?,
        serde_json::to_value(&original)?
    );
    let tools = archive
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 4);
    assert!(
        tools
            .iter()
            .all(|m| !m.is_error && m.content.len() == 48002)
    );
    let bytes = tools.iter().map(|m| m.content.len()).sum::<usize>();
    assert_eq!(bytes, 192008);
    let tokenizer = tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
    let tokens = tools
        .iter()
        .map(|m| tokenizer.encode_ordinary(&m.content).len())
        .sum::<usize>();
    assert!(tokens > 32000);
    f.assert_summarized_continuation(
        &original,
        latest,
        &vec!["piece ".repeat(8000); 4],
        24000,
        true,
    )?;
    eprintln!(
        "one assistant round, four legal JS results: {bytes} bytes, {tokens} tokenizer tokens; no combined result cap"
    );
    Ok(())
}

#[test]
fn raw_sixty_kib_results_need_a_token_budget_in_addition_to_the_byte_backstop() -> Result<()> {
    let tokenizer = tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
    let mut state = 7u64;
    let alphabet =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!@#$%^&*()_+-=[]{}:,.?/|~";
    let random = (0..60000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            alphabet[state as usize % alphabet.len()] as char
        })
        .collect::<String>();
    let cases = [
        ("spaced ascii", "x ".repeat(30000)),
        ("random ascii", random),
        ("digits", "1234567890".repeat(6000)),
        ("escaped unicode", "\u{1F600}\n".repeat(8500)),
    ];
    let mut largest = 0;
    for (name, text) in cases {
        let serialized = serde_json::to_string(&text)?;
        assert!(serialized.len() <= 61440);
        let tokens = tokenizer.encode_ordinary(&serialized).len();
        largest = largest.max(tokens);
        eprintln!(
            "{name}: {} serialized bytes, {tokens} tokenizer tokens",
            serialized.len()
        );
    }
    assert!(largest > 40000);
    Ok(())
}

#[tokio::test]
async fn default_272k_window_bounds_one_two_five_or_ten_large_results_then_summarizes_older_prefix()
-> Result<()> {
    for cell_count in [1, 2, 5, 10] {
        let f = Fixture::new(256000, Some(272000))?;
        let original = f.seed("x ".repeat(247000))?;
        let code = "return 'x '.repeat(30000);";
        f.provider.schedule(&vec![code; cell_count]);
        let latest = "Keep this exact latest instruction and continue after the large actions.";
        let run = f.prompt(latest).await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
        assert!(f.store.context_view(&f.session)?.is_some());
        assert_eq!(f.provider.summary_count(), 0);
        let archive = f.store.messages(&f.session)?;
        assert_eq!(
            serde_json::to_value(&archive[..original.len()])?,
            serde_json::to_value(original)?
        );
        let tools = archive
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), cell_count);
        let mut result_tokens = 0;
        for tool in &tools {
            assert!(!tool.is_error);
            let tokens = estimated_output_tokens(&tool.content)?;
            assert!(tokens <= 10000, "Result has {tokens} estimated tokens");
            result_tokens += tokens;
            let receipt: Value = serde_json::from_str(&tool.content)?;
            assert_eq!(receipt["truncated"], true);
            assert_eq!(spilled_response(&f, &receipt)?.len(), 60002);
        }
        assert!(result_tokens <= cell_count * 10000);
        let requests = f.provider.requests.lock().unwrap();
        let first = requests.first().unwrap();
        let before = f.policy.estimated_tokens(first);
        assert!((247000..256000).contains(&before));
        let resumed = requests.last().unwrap();
        assert_eq!(
            resumed
                .messages
                .iter()
                .filter(|m| m.role == Role::Tool)
                .count(),
            cell_count
        );
        assert!(
            resumed
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content == latest)
        );
        let after = f.policy.estimated_tokens(resumed);
        assert!(after < 256000);
        let frontier = f
            .store
            .context_view_frontier_message_id(
                &f.session,
                f.store.context_view(&f.session)?.unwrap().frontier,
            )?
            .unwrap();
        assert_ne!(frontier, tools.last().unwrap().id);
        eprintln!(
            "default272k, {cell_count} dense JS result(s): pre-request {before}, combined bounded outputs {result_tokens}, prepared after summary {after} estimated tokens; latest complete tool group retained"
        );
    }
    Ok(())
}

#[path = "context_growth/chapter_compat.rs"]
mod chapter_compat;

#[path = "context_growth/small_window.rs"]
mod small_window;

// These tests exercise gathering/apply with deterministic episode-only models.
// Fork transport is covered separately by captured-request adapter tests.
