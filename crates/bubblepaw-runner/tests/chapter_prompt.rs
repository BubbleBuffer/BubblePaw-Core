//! Chapter prompt choice follows the ordinary config layers and defaults to v1.
use anyhow::Result;
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions, Override},
    episodes::ChapterPrompt,
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

fn selected(options: LoadOptions) -> Result<ChapterPrompt> {
    Ok(EffectiveConfig::load(options)?
        .settings()?
        .lcm
        .episodes
        .chapter_prompt)
}

#[test]
fn chapter_prompt_defaults_to_v1_and_follows_all_config_layers() -> Result<()> {
    let root = tempfile::tempdir()?;
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    let mut opts = options(&workspace);
    opts.home = Some(home.clone());
    assert_eq!(selected(opts.clone())?, ChapterPrompt::V1);

    std::fs::create_dir_all(home.join(".bubblepaw"))?;
    std::fs::write(
        home.join(".bubblepaw/config.toml"),
        "[context.lcm]\nchapter_prompt='v2'\n",
    )?;
    assert_eq!(selected(opts.clone())?, ChapterPrompt::V2);

    std::fs::create_dir_all(workspace.join(".bubblepaw"))?;
    std::fs::write(
        workspace.join(".bubblepaw/config.toml"),
        "[context.lcm]\nchapter_prompt='v1'\n",
    )?;
    assert_eq!(selected(opts.clone())?, ChapterPrompt::V1);

    opts.env
        .push(("BUBBLEPAW_CONTEXT__LCM__CHAPTER_PROMPT".into(), "v2".into()));
    assert_eq!(selected(opts.clone())?, ChapterPrompt::V2);
    opts.cli = vec![Override::set("context.lcm.chapter_prompt='v1'")?];
    let config = EffectiveConfig::load(opts)?;
    assert_eq!(
        config.settings()?.lcm.episodes.chapter_prompt,
        ChapterPrompt::V1
    );
    assert_eq!(config.source("context.lcm.chapter_prompt"), Some("CLI"));
    let shown: toml::Value = config.render()?.parse()?;
    assert_eq!(
        shown["context"]["lcm"]["chapter_prompt"].as_str(),
        Some("v1")
    );
    Ok(())
}

#[test]
fn chapter_prompt_rejects_values_outside_the_closed_choice() -> Result<()> {
    let root = tempfile::tempdir()?;
    for value in ["'v0'", "'v3'", "'V2'", "2", "true"] {
        let mut opts = options(root.path());
        opts.cli = vec![Override::set(&format!(
            "context.lcm.chapter_prompt={value}"
        ))?];
        assert!(selected(opts).is_err(), "accepted {value}");
    }
    let mut opts = options(root.path());
    opts.env = vec![("BUBBLEPAW_CONTEXT__LCM__CHAPTER_PROMPT".into(), "v3".into())];
    assert!(selected(opts).is_err());
    Ok(())
}
