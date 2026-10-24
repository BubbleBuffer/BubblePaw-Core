//! Frozen production episodes, shared candidate boundaries, isolated chapter arms.
mod arms;
mod episodes;
mod input;
mod meter;
mod metered_provider;
mod report;
mod scripted;
#[cfg(test)]
mod tests;

use crate::{
    episode_models::{EpisodeModelConfig, HttpEpisodeModels},
    model_roles::ModelRoles,
    provider::ProviderRegistry,
    summarization::SummaryConfig,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, path::PathBuf, sync::Arc};

pub const SUMMARY_MODEL: &str = "opencode/deepseek-v4.1-flash";

#[derive(Clone, Debug)]
pub struct BenchOptions {
    pub chats: Vec<PathBuf>,
    pub gold: PathBuf,
    pub out: PathBuf,
    pub arms: Vec<String>,
    pub dry_run: bool,
    pub refresh_episodes: bool,
    pub max_cost_usd: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Arm {
    pub name: String,
    pub kind: String,
    pub model: String,
}

pub(crate) fn parse_arm(value: &str) -> Result<Arm> {
    let (name, route) = value
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("Arm needs name=decider:<model>"))?;
    let (kind, model) = route
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("Arm needs a kind and model"))?;
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c)),
        "Arm name must contain letters, digits, hyphens or underscores"
    );
    ensure!(
        kind == "decider" && !model.is_empty(),
        "Invalid arm kind or model"
    );
    Ok(Arm {
        name: name.into(),
        kind: kind.into(),
        model: model.into(),
    })
}

pub fn scripted_registry(options: &BenchOptions) -> Result<Arc<ProviderRegistry>> {
    scripted::registry(options)
}

pub async fn run(
    options: BenchOptions,
    registry: Arc<ProviderRegistry>,
    summary: SummaryConfig,
) -> Result<Value> {
    ensure!(
        options.max_cost_usd.is_finite() && options.max_cost_usd >= 0.0,
        "Invalid cost cap"
    );
    ensure!(
        !options.chats.is_empty() && !options.arms.is_empty(),
        "At least one chat and arm required"
    );
    let arms = options
        .arms
        .iter()
        .map(|a| parse_arm(a))
        .collect::<Result<Vec<_>>>()?;
    let mut names = HashSet::new();
    ensure!(
        arms.iter().all(|a| names.insert(&a.name)),
        "Duplicate arm name"
    );
    let corpora = options
        .chats
        .iter()
        .map(|p| input::load(p, &options.gold))
        .collect::<Result<Vec<_>>>()?;
    let mut names = HashSet::new();
    ensure!(
        corpora.iter().all(|c| names.insert(&c.chat_id)),
        "Duplicate chat ID"
    );
    std::fs::create_dir_all(&options.out)?;
    let meter = Arc::new(meter::Meter::open(
        &options.out,
        options.max_cost_usd,
        options.dry_run,
    )?);
    let registry = metered_provider::wrap(registry, meter.clone(), &arms)?;
    let mut roles = ModelRoles::for_models(SUMMARY_MODEL.into(), String::new());
    roles.summary.models = vec![SUMMARY_MODEL.into()];
    roles.chapter.models = vec![SUMMARY_MODEL.into()];
    let models = Arc::new(
        HttpEpisodeModels::from_registry(
            EpisodeModelConfig {
                roles: roles.clone(),
                ..Default::default()
            },
            registry.clone(),
            None,
        )?
        .with_episode_config(summary.lcm.episodes.clone())?,
    );
    let mut stages = Vec::new();
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for corpus in &corpora {
        meter.scope("episodes", "", &corpus.chat_id, "", "");
        let stage_meter = meter.clone();
        let frozen = episodes::generate(
            corpus,
            &options.out,
            options.dry_run,
            options.refresh_episodes,
            &json!({"summary": SUMMARY_MODEL}),
            &move |id| stage_meter.stage(id),
            models.clone(),
            summary.clone(),
        )
        .await;
        match frozen {
            Ok(frozen) => stages.push((corpus, frozen)),
            Err(error) => {
                failures.push(
                    json!({"stage":"episodes","chat":corpus.chat_id,"error":format!("{error:#}")}),
                );
                break;
            }
        }
    }
    if failures.is_empty() {
        'arms: for arm in &arms {
            let mut arm_roles = roles.clone();
            arm_roles.decider.models = vec![arm.model.clone()];
            let models = Arc::new(
                HttpEpisodeModels::from_registry(
                    EpisodeModelConfig {
                        roles: arm_roles,
                        ..Default::default()
                    },
                    registry.clone(),
                    None,
                )?
                .with_episode_config(summary.lcm.episodes.clone())?,
            );
            for (corpus, frozen) in &stages {
                meter.scope("arm", &arm.name, &corpus.chat_id, &frozen.stage_id, "");
                let call_meter = meter.clone();
                match arms::predictions(
                    corpus,
                    frozen,
                    arm,
                    models.clone(),
                    &move |id| call_meter.candidate(id),
                    summary.lcm.episodes.decision_context_tokens,
                )
                .await
                {
                    Ok(predicted) => results.push(report::score(corpus, arm, predicted)),
                    Err(error) => {
                        failures.push(json!({"stage":"arm","arm":arm.name,"chat":corpus.chat_id,"error":format!("{error:#}")}));
                        if meter.blocked() {
                            break 'arms;
                        }
                    }
                }
            }
        }
    }
    let value = report::write(&options, &arms, &stages, results, failures, &meter)?;
    Ok(value)
}
