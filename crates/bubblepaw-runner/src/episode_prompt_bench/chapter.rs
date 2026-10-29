//! Compare chapter prompt v1/v2 over frozen episode memories, without regenerating
//! episodes. Input JSON is `{ "fixtures": [{ "id": "case", "session_id": "chat",
//! "episodes": ["EPISODE 1 [source a..b]\n..."], "typed_user_texts": ["..."],
//! "task_instructions": [], "previous_chapters": [] }] }`.
//! The scripted mode tests production request construction and parsing, not quality.
use anyhow::{Context, Result, ensure};
use bubblepaw_core::ContextSubjectChapter;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Instant};

use crate::{
    episode_models::{ChapterInput, EpisodeModelConfig, HttpEpisodeModels},
    episodes::{ChapterPrompt, EpisodeConfig},
    model_roles::ModelRoles,
    provider::{ProviderRegistry, ScriptedProvider, ScriptedStep},
};

pub struct ChapterBenchOptions {
    pub fixtures: PathBuf,
    pub out: PathBuf,
    pub dry_run: bool,
    pub max_cost_usd: f64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    id: String,
    session_id: String,
    episodes: Vec<String>,
    #[serde(default)]
    previous_chapters: Vec<ContextSubjectChapter>,
    #[serde(default)]
    typed_user_texts: Vec<String>,
    #[serde(default)]
    task_instructions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixtures {
    fixtures: Vec<Fixture>,
}

fn load(path: &std::path::Path) -> Result<Vec<Fixture>> {
    let input: Fixtures = serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("chapter fixtures {}", path.display()))?;
    ensure!(
        !input.fixtures.is_empty(),
        "at least one fixture is required"
    );
    let mut ids = HashSet::new();
    for fixture in &input.fixtures {
        ensure!(
            !fixture.id.trim().is_empty() && ids.insert(&fixture.id),
            "fixture IDs must be nonempty and unique"
        );
        ensure!(
            !fixture.session_id.trim().is_empty() && !fixture.episodes.is_empty(),
            "fixture {} needs a session and episode memories",
            fixture.id
        );
        let text = serde_json::to_string(fixture)?;
        ensure!(
            !super::looks_secret(&text),
            "credential-like text in fixture {}",
            fixture.id
        );
    }
    Ok(input.fixtures)
}

fn models(registry: Arc<ProviderRegistry>, prompt: ChapterPrompt) -> Result<HttpEpisodeModels> {
    let mut roles = ModelRoles::for_models(super::SUMMARY_MODEL.into(), String::new());
    roles.chapter.models = vec![super::SUMMARY_MODEL.into()];
    HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            ..Default::default()
        },
        registry,
        None,
    )?
    .with_episode_config(EpisodeConfig {
        chapter_prompt: prompt,
        ..Default::default()
    })
}

fn scripted_text(fixture: &Fixture, structured: bool) -> Result<String> {
    let mut text =
        "# Scripted chapter\nSynthetic chapter output for transport and parser verification."
            .to_owned();
    if structured {
        let ordinal = |episode: &str| -> Result<usize> {
            episode
                .strip_prefix("EPISODE ")
                .and_then(|rest| rest.split_whitespace().next())
                .context("scripted fixtures need EPISODE n [source ...] headers")?
                .parse()
                .context("invalid fixture episode ordinal")
        };
        let first = ordinal(&fixture.episodes[0])?;
        let last = ordinal(fixture.episodes.last().context("no fixture episodes")?)?;
        text.push_str(&format!(
            "\n\nDecisions: none\n\nSections:\n- Fixture work | episodes {first}-{last}"
        ));
    }
    text.push_str("\nRelated: none");
    Ok(text)
}

fn scripted_registry(fixtures: &[Fixture]) -> Result<Arc<ProviderRegistry>> {
    let mut steps = Vec::new();
    for fixture in fixtures {
        for structured in [false, true] {
            steps.push(ScriptedStep::Text {
                text: scripted_text(fixture, structured)?,
            });
        }
    }
    Ok(Arc::new(
        ProviderRegistry::new(
            "opencode",
            Arc::new(ScriptedProvider::with_steps(steps, "")),
        )
        .with_strict_routes(),
    ))
}

/// `dry_run` always constructs its own scripted registry. Live mode requires an
/// explicit configured registry. Neither path opens a chat store or executes tools.
pub async fn run(
    options: ChapterBenchOptions,
    registry: Option<Arc<ProviderRegistry>>,
) -> Result<Value> {
    ensure!(
        options.max_cost_usd.is_finite() && options.max_cost_usd > 0.0,
        "max-cost-usd must be finite and positive"
    );
    let fixtures = load(&options.fixtures)?;
    let registry = if options.dry_run {
        scripted_registry(&fixtures)?
    } else {
        registry.context("live chapter comparison requires a provider registry")?
    };
    let arms = [
        ("v1", models(registry.clone(), ChapterPrompt::V1)?),
        ("v2", models(registry, ChapterPrompt::V2)?),
    ];
    std::fs::create_dir_all(&options.out)?;
    let mut spend = super::Spend {
        usd: 0.0,
        cap: options.max_cost_usd,
        calls: Vec::new(),
    };
    let mut records = Vec::new();
    for fixture in fixtures {
        let shared = serde_json::to_value(&fixture)?;
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&shared)?));
        let input = ChapterInput {
            session_id: fixture.session_id.clone(),
            episodes: fixture.episodes.clone(),
            previous_chapters: fixture.previous_chapters.clone(),
            typed_user_texts: fixture.typed_user_texts.clone(),
            task_instructions: fixture.task_instructions.clone(),
        };
        let mut outputs = serde_json::Map::new();
        for (name, models) in &arms {
            spend.check()?;
            let started = Instant::now();
            let (result, metadata_text, metadata_error) = models
                .close_chapter_with_metadata(input.clone())
                .await
                .with_context(|| format!("{name} chapter for fixture {}", fixture.id))?;
            if options.dry_run {
                spend.calls.push(json!({"kind": "chapter", "arm": name,
                    "fixture": fixture.id, "cost_usd": 0.0}));
            } else {
                spend.record("chapter", name, &result.model, &result.usage)?;
            }
            outputs.insert(
                (*name).into(),
                json!({"input_sha256": fingerprint, "description": result.description,
                    "summary": result.summary, "decisions": result.decisions,
                    "sections": result.sections, "metadata_text": metadata_text,
                    "metadata_error": metadata_error, "related_ids": result.related_ids,
                    "related_evidence": result.related_evidence, "model": result.model,
                    "usage": result.usage, "elapsed_ms": started.elapsed().as_millis()}),
            );
        }
        records.push(json!({"input": shared, "input_sha256": fingerprint, "arms": outputs}));
    }
    let report = json!({"schema": "bubblepaw.chapter-prompt-bench.v1",
        "acceptance": if options.dry_run { "scripted" } else { "live_model" },
        "model": super::SUMMARY_MODEL, "fixtures": records,
        "spend_usd": spend.usd, "calls": spend.calls});
    std::fs::write(
        options.out.join("chapter-prompt-bench.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(json!({"fixtures": records.len(), "arms": ["v1", "v2"],
        "acceptance": report["acceptance"], "spend_usd": spend.usd}))
}

#[cfg(test)]
#[path = "chapter/tests.rs"]
mod tests;
