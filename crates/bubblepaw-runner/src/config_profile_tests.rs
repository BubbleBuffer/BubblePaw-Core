//! Checked-in worker profile resolution and explicit main fallbacks.
use super::*;

fn options(root: &Path) -> LoadOptions {
    LoadOptions {
        workspace: root.join("workspace"),
        home: Some(root.join("home")),
        custom_file: None,
        env: vec![],
        cli: vec![],
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn explicit_main_fallbacks_validate_and_worker_profile_resolves() {
    let root = tempfile::tempdir().unwrap();
    for value in [
        "['unqualified']",
        "['missing/model']",
        "['codex/gpt-6-luna']",
        "['opencode/flash','opencode/flash']",
    ] {
        let mut opts = options(root.path());
        opts.cli
            .push(Override::set(&format!("model.fallbacks={value}")).unwrap());
        assert!(
            EffectiveConfig::load(opts)
                .and_then(|config| config.settings())
                .is_err(),
            "{value}"
        );
    }
    let mut opts = options(root.path());
    opts.cli
        .push(Override::set("model.fallbacks=['opencode/deepseek-v4.1-flash']").unwrap());
    assert_eq!(
        EffectiveConfig::load(opts)
            .unwrap()
            .settings()
            .unwrap()
            .main_fallbacks,
        ["opencode/deepseek-v4.1-flash"]
    );
    let profile = include_str!("../../../examples/worker-opencode-go.toml");
    write(
        &options(root.path())
            .workspace
            .join(".bubblepaw/config.toml"),
        profile,
    );
    let settings = EffectiveConfig::load(options(root.path()))
        .unwrap()
        .settings()
        .unwrap();
    assert_eq!(
        settings.main_model,
        "opencode-free/deepseek-v4.1-flash-free"
    );
    assert_eq!(settings.main_fallbacks, ["opencode/deepseek-v4.1-flash"]);
    assert_eq!(
        settings.roles.decider.models,
        [
            "opencode-decisions/jev-1.13-free",
            "opencode-decisions/jev-1.13",
            "llm/opencode/deepseek-v4.1-flash"
        ]
    );
    assert_eq!(
        settings.roles.approver.models,
        settings.roles.decider.models
    );
    assert_eq!(settings.roles.decider.repeats, Some(1));
    assert_eq!(settings.roles.decider.threshold, Some(0.0));
    assert_eq!(settings.context_window_tokens, Some(200000));
    assert_eq!(settings.compact_threshold_tokens, 200000);
    assert_eq!(settings.lcm.child_compact_threshold_tokens, Some(100000));
    assert_eq!(settings.lcm.episodes.chapter_rollover_tokens, 0);
}

#[test]
fn approvals_auto_prompt_layers_over_the_built_in_default() {
    let root = tempfile::tempdir().unwrap();
    let defaults = EffectiveConfig::load(options(root.path()))
        .unwrap()
        .settings()
        .unwrap();
    assert_eq!(
        crate::model_roles::approver_prompt(&defaults.approvals),
        bubblepaw_core::DEFAULT_AUTO_PROMPT
    );
    let custom = root.path().join("prompt.toml");
    write(
        &custom,
        "[approvals]\nauto_prompt = \"Is {tool} fine for {user_request}?\"\n",
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(custom);
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    assert_eq!(
        crate::model_roles::approver_prompt(&settings.approvals),
        "Is {tool} fine for {user_request}?"
    );
}

#[test]
fn approver_repeats_and_threshold_are_rejected_with_a_clear_message() {
    let root = tempfile::tempdir().unwrap();
    for entry in ["roles.approver.threshold=0.9", "roles.approver.repeats=1"] {
        let mut opts = options(root.path());
        opts.cli.push(Override::set(entry).unwrap());
        let message = format!("{:#}", EffectiveConfig::load(opts).err().unwrap());
        assert!(
            message.contains("the approver makes one yes/no decision; remove the key"),
            "{message}"
        );
    }
    let mut opts = options(root.path());
    opts.cli
        .push(Override::set("roles.decider.threshold=0.7").unwrap());
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    assert_eq!(settings.roles.decider.threshold, Some(0.7));
}
