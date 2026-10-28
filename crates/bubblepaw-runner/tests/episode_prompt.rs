//! `context.lcm.episode_prompt` selection: v2 by default, v3 only when configured.
use anyhow::Result;
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions, Override},
    episodes::EpisodePrompt,
};

fn options(workspace: &std::path::Path) -> LoadOptions {
    LoadOptions {
        workspace: workspace.into(),
        home: None,
        custom_file: None,
        env: vec![],
        cli: vec![],
    }
}

#[test]
fn episode_prompt_defaults_to_v2_and_layers_as_a_closed_choice() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut opts = options(root.path());
    let default = EffectiveConfig::load(opts.clone())?;
    assert_eq!(
        default.settings()?.lcm.episodes.episode_prompt,
        EpisodePrompt::V2
    );
    opts.cli = vec![Override::set("context.lcm.episode_prompt='v3'")?];
    let selected = EffectiveConfig::load(opts.clone())?;
    assert_eq!(
        selected.settings()?.lcm.episodes.episode_prompt,
        EpisodePrompt::V3
    );
    let shown: toml::Value = selected.render()?.parse()?;
    assert_eq!(
        shown["context"]["lcm"]["episode_prompt"].as_str(),
        Some("v3")
    );
    opts.cli = vec![Override::set("context.lcm.episode_prompt='v4'")?];
    assert!(
        EffectiveConfig::load(opts)
            .and_then(|config| config.settings())
            .is_err()
    );
    Ok(())
}

#[test]
fn v3_prompt_adds_the_gist_and_action_log_but_keeps_the_length_limits() {
    let v2 = EpisodePrompt::V2.text();
    let v3 = EpisodePrompt::V3.text();
    assert!(!v2.contains("Actions:"));
    for needle in [
        "{target}",
        "{limit}",
        "at most 25 words",
        "\"Actions:\" section immediately before \"Open:\"",
        "<Verb> <target>[ · <change>][ · <outcome>][ [<call-id>]][ ! <kind>: <reason>]",
        "decision, error, correction, question, risk",
        "at most 12 lines",
        "\"Actions: none\"",
    ] {
        assert!(v3.contains(needle), "{needle}");
    }
}
