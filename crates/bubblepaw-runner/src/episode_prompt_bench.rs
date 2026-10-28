//! Dev-only comparison of episode summary prompts over recorded chats: plans each
//! chat's episodes as the live gatherer does, summarizes the same episode-only input
//! once per prompt version, and closes one chapter over the v3 summaries.
//! Reads a copy of a store; writes only JSON under `out`.
#[path = "episode_prompt_bench/chapter.rs"]
pub mod chapter;

use anyhow::{Context, Result, ensure};
use bubblepaw_core::{MessageOrigin, Store, Usage, is_typed_user};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

use crate::{
    episode_models::{
        ChapterInput, EpisodeModelConfig, EpisodeModels, HttpEpisodeModels, SummaryInput,
    },
    episode_worker::{render_episode_range, render_task_anchor},
    episodes::{EpisodeConfig, EpisodePrompt},
    history_snapshot::HistorySnapshot,
    model_roles::ModelRoles,
    provider::{ProviderRegistry, ScriptedProvider},
    summarization::{SummarizingContext, SummaryConfig},
};

pub const SUMMARY_MODEL: &str = "opencode/deepseek-v4.1-flash";

pub struct BenchOptions {
    /// A copy of a data dir (`catalog.db` + `chats/`); the original store is never opened.
    pub data_dir: PathBuf,
    pub sessions: Vec<String>,
    pub episodes: usize,
    /// At most this many episodes from one chat.
    pub per_session: usize,
    /// Prompt arms, each summarizing every episode; a `v3` arm also closes one chapter.
    pub arms: Vec<EpisodePrompt>,
    /// Also summarize a chat's unclosed tail as its last episode (labelled `tail`).
    pub tail: bool,
    pub out: PathBuf,
    pub max_cost_usd: f64,
}

/// Inputs containing anything credential-like are skipped, never sent.
fn looks_secret(text: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            regex::Regex::new(
                r"(?i)api[_-]?key\s*[:=]|secret\s*[:=]|password\s*[:=]|token=|bearer\s+[a-z0-9._-]{16}|sk-[a-z0-9_-]{16}",
            )
            .unwrap()
        })
        .is_match(text)
}

struct Spend {
    usd: f64,
    cap: f64,
    calls: Vec<Value>,
}
impl Spend {
    fn record(&mut self, kind: &str, arm: &str, model: &str, usage: &Usage) -> Result<()> {
        let cost = crate::pricing::PriceTable::shared()
            .usage_cost(model, usage, None)
            .with_context(|| format!("no price for {model}"))?;
        self.usd += cost;
        self.calls
            .push(json!({"kind": kind, "arm": arm, "model": model,
            "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens,
            "cache_read_tokens": usage.cache_read_tokens, "cost_usd": cost}));
        Ok(())
    }
    fn check(&self) -> Result<()> {
        ensure!(
            self.usd < self.cap,
            "spend {:.4} USD reached the {:.2} USD cap",
            self.usd,
            self.cap
        );
        Ok(())
    }
}

fn models(registry: &Arc<ProviderRegistry>, prompt: EpisodePrompt) -> Result<HttpEpisodeModels> {
    let mut roles = ModelRoles::for_models(SUMMARY_MODEL.into(), String::new());
    roles.summary.models = vec![SUMMARY_MODEL.into()];
    roles.chapter.models = vec![SUMMARY_MODEL.into()];
    HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            ..Default::default()
        },
        registry.clone(),
        None,
    )?
    .with_episode_config(EpisodeConfig {
        episode_prompt: prompt,
        ..Default::default()
    })
}

pub async fn run(options: BenchOptions, registry: Arc<ProviderRegistry>) -> Result<Value> {
    ensure!(options.max_cost_usd.is_finite() && options.max_cost_usd > 0.0);
    std::fs::create_dir_all(&options.out)?;
    ensure!(
        !options.arms.is_empty(),
        "at least one prompt arm is required"
    );
    let arms = options
        .arms
        .iter()
        .map(|prompt| {
            let name = serde_json::to_value(prompt)?
                .as_str()
                .context("prompt name")?
                .to_owned();
            Ok((name, models(&registry, *prompt)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let chapter_arm = arms.iter().position(|(name, _)| name == "v3");
    let config = EpisodeConfig::default();
    let store = Store::open_data_dir(&options.data_dir)?;
    let mut spend = Spend {
        usd: 0.0,
        cap: options.max_cost_usd,
        calls: Vec::new(),
    };
    let mut episodes = Vec::new();
    let mut skipped = Vec::new();
    let mut chapter = Value::Null;
    // Token counts are lazy; scan once with the default estimator so planning
    // sees real sizes, as the live worker does before it plans.
    let estimator = SummarizingContext::new(
        store.clone(),
        Arc::new(ScriptedProvider::new(
            "Bench estimator must not call a model",
        )),
        SummaryConfig::default(),
        None,
    )?;
    'sessions: for session in &options.sessions {
        let mut snapshot = HistorySnapshot::index(&store, session)?;
        snapshot.scan(0, &estimator)?;
        let mut coverage = 0;
        let mut previous: Vec<Option<String>> = vec![None; arms.len()];
        let mut taken = 0;
        let mut v3_members = Vec::new();
        let mut typed = Vec::new();
        let mut tasks = Vec::new();
        while episodes.len() < options.episodes && taken < options.per_session {
            let plan = snapshot.plan_with_config(coverage, &config)?;
            // A finished chat's unclosed tail is summarized as its last episode.
            let tail_end = plan.remainder.as_ref().map(|rest| rest.end);
            let (planned, tail) = match plan.episodes.into_iter().next() {
                Some(planned) => (planned, false),
                None => match tail_end.and_then(|end| snapshot.manual_episode(coverage, end)) {
                    Some(planned) => (planned, true),
                    None => break,
                },
            };
            if tail && !options.tail {
                break;
            }
            let range = planned.message_range.clone();
            coverage = range.end;
            let turn_request = planned
                .origin_request_id
                .as_ref()
                .map(|id| {
                    snapshot
                        .message(snapshot.position(id)?)
                        .map(|m| render_task_anchor(&m))
                })
                .transpose()?;
            let episode_text = render_episode_range(
                &snapshot,
                range.start,
                range.end,
                config.tool_evidence_tokens,
            )?;
            let mut tool_calls = Vec::new();
            let mut tool_results = Vec::new();
            let mut members_typed = Vec::new();
            let mut members_tasks = Vec::new();
            snapshot.walk(range.start, range.end, |_, message| {
                for call in &message.tool_calls {
                    tool_calls.push(
                        json!({"id": call.id, "name": call.name, "arguments": call.arguments}),
                    );
                }
                if let Some(id) = &message.tool_call_id {
                    tool_results.push(json!({"id": id, "content": message.content}));
                }
                if is_typed_user(&message) {
                    members_typed.push(message.content.clone());
                } else if message.origin == Some(MessageOrigin::ChildTask) {
                    members_tasks.push(message.content.clone());
                }
                Ok(())
            })?;
            let label = json!({"session": session, "first": range.first_message_id,
                "last": range.last_message_id, "source_tokens": planned.source_tokens,
                "tail": tail});
            if looks_secret(&episode_text) || turn_request.as_deref().is_some_and(looks_secret) {
                skipped.push(json!({"episode": label, "reason": "credential-like text"}));
                previous = vec![None; arms.len()];
                continue;
            }
            let mut record = json!({"episode": label, "turn_request": turn_request,
                "source": episode_text, "tool_calls": tool_calls, "tool_results": tool_results});
            for (index, (name, models)) in arms.iter().enumerate() {
                spend.check()?;
                let result = models
                    .summarize(SummaryInput {
                        focus: None,
                        session_id: session.clone(),
                        previous_summary: previous[index].clone(),
                        turn_request: turn_request.clone(),
                        episode_text: episode_text.clone(),
                        cap: config.summary_target_tokens,
                    })
                    .await
                    .with_context(|| format!("{name} summary for {session}"))?;
                spend.record("summary", name, &result.model, &result.usage)?;
                record[name.as_str()] = json!({"summary": result.summary, "model": result.model,
                    "output_tokens": result.usage.output_tokens,
                    "input_tokens": result.usage.input_tokens});
                previous[index] = Some(result.summary.clone());
                if name == "v3" {
                    v3_members.push(format!(
                        "EPISODE {} [source {}..{}]\n{}",
                        v3_members.len() + 1,
                        range.first_message_id,
                        range.last_message_id,
                        result.summary
                    ));
                }
            }
            typed.extend(members_typed);
            tasks.extend(members_tasks);
            episodes.push(record);
            taken += 1;
        }
        if let Some(arm) = chapter_arm
            && chapter.is_null()
            && v3_members.len() >= 2
        {
            spend.check()?;
            let result = arms[arm]
                .1
                .close_chapter(ChapterInput {
                    session_id: session.clone(),
                    episodes: v3_members.clone(),
                    previous_chapters: Vec::new(),
                    typed_user_texts: typed,
                    task_instructions: tasks,
                })
                .await?;
            spend.record("chapter", "v3", &result.model, &result.usage)?;
            chapter = json!({"session": session, "episodes": v3_members.len(),
                "description": result.description, "summary": result.summary,
                "output_tokens": result.usage.output_tokens});
        }
        if episodes.len() >= options.episodes {
            break 'sessions;
        }
    }
    let report = json!({"model": SUMMARY_MODEL, "episodes": episodes, "skipped": skipped,
        "chapter": chapter, "spend_usd": spend.usd, "calls": spend.calls});
    std::fs::write(
        options.out.join("episode-prompt-bench.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(
        json!({"episodes": report["episodes"].as_array().map(Vec::len),
        "skipped": report["skipped"].as_array().map(Vec::len),
        "chapter": !report["chapter"].is_null(), "spend_usd": report["spend_usd"]}),
    )
}
