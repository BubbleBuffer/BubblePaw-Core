//! Dev-only: compare episode summary prompts v2 and v3 on recorded chats.
//! `bubblepaw-episode-prompt-bench --data-dir <store copy> --out <dir> [--config <toml>] [--episodes 8]
//! [--per-session N] [--arms v2,v3] [--no-tail] [--max-cost-usd 2] <session>...`.
//! Live calls use the summary route's normal credentials.
//! Chapter-only comparison: `--chapter-fixtures <json> --out <dir> [--dry-run]`.
use anyhow::{Context, Result, bail};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions},
    episode_prompt_bench::{self, BenchOptions},
    episodes::EpisodePrompt,
    provider::build_provider_registry,
};
use std::{path::PathBuf, sync::Arc};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut data_dir, mut out, mut config, mut episodes, mut cap) = (None, None, None, 8, 2.0);
    let (mut per_session, mut tail) = (usize::MAX, true);
    let mut arms = vec![EpisodePrompt::V2, EpisodePrompt::V3];
    let mut sessions = Vec::new();
    let (mut chapter_fixtures, mut dry_run) = (None, false);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--config" => config = Some(PathBuf::from(value()?)),
            "--episodes" => episodes = value()?.parse()?,
            "--max-cost-usd" => cap = value()?.parse()?,
            "--per-session" => per_session = value()?.parse()?,
            "--arms" => {
                arms = value()?
                    .split(',')
                    .map(|name| Ok(serde_json::from_value(name.trim().into())?))
                    .collect::<Result<_>>()?
            }
            "--no-tail" => tail = false,
            "--chapter-fixtures" => chapter_fixtures = Some(PathBuf::from(value()?)),
            "--dry-run" => dry_run = true,
            flag if flag.starts_with("--") => bail!("unknown flag {flag}"),
            _ => sessions.push(arg),
        }
    }
    if let Some(fixtures) = chapter_fixtures {
        anyhow::ensure!(
            data_dir.is_none() && sessions.is_empty(),
            "--chapter-fixtures cannot be combined with a data dir or sessions"
        );
        // Resolve scripted mode before reading configuration or constructing HTTP providers.
        let registry = if dry_run {
            None
        } else {
            Some(load_registry(config)?)
        };
        let summary = episode_prompt_bench::chapter::run(
            episode_prompt_bench::chapter::ChapterBenchOptions {
                fixtures,
                out: out.context("--out is required")?,
                dry_run,
                max_cost_usd: cap,
            },
            registry,
        )
        .await?;
        println!("{summary}");
        return Ok(());
    }
    anyhow::ensure!(!dry_run, "--dry-run requires --chapter-fixtures");
    let registry = load_registry(config)?;
    let summary = episode_prompt_bench::run(
        BenchOptions {
            data_dir: data_dir.context("--data-dir is required")?,
            sessions,
            episodes,
            per_session,
            arms,
            tail,
            out: out.context("--out is required")?,
            max_cost_usd: cap,
        },
        registry,
    )
    .await?;
    println!("{summary}");
    Ok(())
}

fn load_registry(
    config: Option<PathBuf>,
) -> Result<Arc<bubblepaw_runner::provider::ProviderRegistry>> {
    let settings = EffectiveConfig::load(LoadOptions {
        workspace: std::env::current_dir()?,
        home: None,
        custom_file: config,
        env: std::env::vars().collect(),
        cli: vec![],
    })?
    .settings()?;
    Ok(Arc::new(build_provider_registry(&settings)?))
}
