//! Feature-gated command parsing kept outside the ordinary runner entrypoint.
use anyhow::{Result, ensure};
use bubblepaw_runner::{
    chapter_bench::{self, BenchOptions},
    config::Settings,
    provider::build_provider_registry,
    summarization::SummaryConfig,
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

#[derive(clap::Args, Clone, Serialize, Deserialize)]
pub(super) struct Args {
    /// Canonical corpus JSON or a brain/chats/<name> directory; repeat for several chats.
    #[arg(long, required = true)]
    chat: Vec<PathBuf>,
    #[arg(long)]
    gold: PathBuf,
    #[arg(long)]
    out: PathBuf,
    /// Named route, for example baseline=decider:opencode-decisions/jev or model=decider:llm/opencode/model.
    #[arg(long, required = true)]
    arm: Vec<String>,
    /// Use scripted responses without reading credentials or making model requests.
    #[arg(long)]
    dry_run: bool,
    /// Regenerate the shared episode cache before evaluating the arms.
    #[arg(long)]
    refresh_episodes: bool,
    #[arg(long, default_value_t = 0.50)]
    max_cost_usd: f64,
}

pub(super) async fn run(args: &Args, settings: &Settings) -> Result<serde_json::Value> {
    ensure!(
        args.max_cost_usd.is_finite() && args.max_cost_usd >= 0.0,
        "max-cost-usd must be finite and nonnegative"
    );
    let options = BenchOptions {
        chats: args.chat.clone(),
        gold: args.gold.clone(),
        out: args.out.clone(),
        arms: args.arm.clone(),
        dry_run: args.dry_run,
        refresh_episodes: args.refresh_episodes,
        max_cost_usd: args.max_cost_usd,
    };
    // Resolve the offline branch before constructing any configured HTTP/extension provider.
    let registry = if args.dry_run {
        chapter_bench::scripted_registry(&options)?
    } else {
        Arc::new(build_provider_registry(settings)?)
    };
    chapter_bench::run(
        options,
        registry,
        SummaryConfig {
            threshold_cap: settings.compact_threshold_tokens,
            context_window: settings.context_window_tokens,
            verbatim_user_tokens: settings.verbatim_user_tokens,
            lcm: settings.lcm.clone(),
            ..SummaryConfig::default()
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn parses_repeated_chats_and_named_arms() {
        let cli = Cli::try_parse_from([
            "bubblepaw",
            "chapter-bench",
            "--chat",
            "first.json",
            "--chat",
            "brain/chats/second",
            "--gold",
            "gold.json",
            "--out",
            "report",
            "--arm",
            "baseline=decider:opencode-decisions/jev",
            "--arm",
            "model=decider:llm/opencode/model",
            "--dry-run",
            "--refresh-episodes",
        ])
        .unwrap();
        let Command::ChapterBench(args) = cli.command else {
            panic!("wrong command")
        };
        assert_eq!(args.chat.len(), 2);
        assert_eq!(args.arm.len(), 2);
        assert_eq!(args.max_cost_usd, 0.50);
        assert!(args.dry_run && args.refresh_episodes);
    }
}
