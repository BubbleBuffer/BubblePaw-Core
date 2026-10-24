//! Scripted skills acceptance. No model or external transport calls.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    Store, TextCallback, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions, Override},
    skills::{MAX_SKILL_BYTES, Skills},
    tools::WorkspaceTools,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[path = "support/notice_fixture.rs"]
mod notice_fixture;
use notice_fixture::Notices;

fn write(path: impl AsRef<Path>, text: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
fn skill(directory: impl AsRef<Path>, name: &str, description: &str, body: &str) {
    write(
        directory.as_ref().join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    );
}
fn registry(store: &Store, workspace: &Path, user: Option<PathBuf>) -> Skills {
    Skills::new(store.clone(), workspace.to_owned(), user)
}

struct Done;
#[async_trait]
impl Provider for Done {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}
fn persist(store: &Store, skills: &Skills, session: &str) -> Vec<String> {
    let notices = skills.take_changes(session);
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(Notices::new("skill_notices", notices.clone())))
        .unwrap();
    let harness = Harness::new(
        store.clone(),
        Arc::new(Done),
        tools,
        HarnessConfig::default(),
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    assert_eq!(
        runtime
            .block_on(harness.prompt(session, "persist notice", RunOptions::default()))
            .unwrap()
            .status,
        "completed"
    );
    notices
}

#[test]
fn project_nearest_first_both_layouts_then_user_and_extra_first_duplicate_wins() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let workspace = repo.join("nested");
    std::fs::create_dir_all(&workspace).unwrap();
    write(repo.join(".git"), "gitdir: elsewhere");
    skill(
        root.path().join(".agents/skills/excluded"),
        "outside",
        "Outside",
        "outside body",
    );
    skill(
        workspace.join(".agents/skills/a"),
        "shared",
        "Leaf agents wins",
        "BODY-NEVER-IN-CATALOG",
    );
    skill(
        workspace.join(".claude/skills/a"),
        "shared",
        "Leaf claude loses",
        "body",
    );
    skill(
        workspace.join(".claude/skills/b"),
        "claude",
        "Leaf Claude",
        "body",
    );
    skill(repo.join(".agents/skills/a"), "root", "Root agents", "body");
    let user = root.path().join("user");
    skill(user.join("a"), "shared", "User loses", "body");
    skill(user.join("b"), "user", "User skill", "body");
    let extra = root.path().join("extra");
    skill(extra.join("a"), "extra", "Extra skill", "body");
    let store = Store::in_memory().unwrap();
    let session = store.create_session("order").unwrap().id;
    let skills = registry(&store, &workspace, Some(user));
    skills.check(&session, true, &[extra]).unwrap();
    let prompt = skills.system_prompt(&session, "base");
    let mut previous = 0;
    for expected in [
        "Leaf agents wins",
        "Leaf Claude",
        "Root agents",
        "User skill",
        "Extra skill",
    ] {
        let position = prompt.find(expected).unwrap();
        assert!(position > previous, "{prompt}");
        previous = position;
    }
    assert!(
        prompt.contains(
            &workspace
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .replace('\\', "\\\\")
        )
    );
    for excluded in [
        "Outside",
        "Leaf claude loses",
        "User loses",
        "BODY-NEVER-IN-CATALOG",
    ] {
        assert!(!prompt.contains(excluded));
    }
    assert!(skills.take_changes(&session).is_empty());
}

#[test]
fn no_git_does_not_search_parent_and_extra_accepts_direct_skill_folder() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    skill(
        root.path().join(".agents/skills/parent"),
        "parent",
        "Parent excluded",
        "body",
    );
    skill(
        workspace.join(".agents/skills/local"),
        "local",
        "Local skill",
        "body",
    );
    let direct = root.path().join("direct");
    skill(&direct, "direct", "Direct folder", "body");
    let store = Store::in_memory().unwrap();
    let session = store.create_session("no git").unwrap().id;
    let skills = registry(&store, &workspace, None);
    skills.check(&session, true, &[direct]).unwrap();
    let prompt = skills.system_prompt(&session, "");
    assert!(prompt.contains("Local skill") && prompt.contains("Direct folder"));
    assert!(!prompt.contains("Parent excluded"));
}

#[test]
fn yaml_quoted_multiline_errors_skipped_once_and_catalog_is_bounded() {
    let root = tempfile::tempdir().unwrap();
    let collection = root.path().join(".agents/skills");
    write(
        collection.join("a/SKILL.md"),
        "---\nname: 'quoted:name'\ndescription: >-\n  A quoted: description\n  over two lines.\n---\nbody\n",
    );
    write(
        collection.join("b/SKILL.md"),
        "---\nname: broken\ndescription: [unclosed\n---\nbody",
    );
    write(collection.join("c/SKILL.md"), "no frontmatter");
    write(
        collection.join("d/SKILL.md"),
        "---\nname: ''\ndescription: absent name\n---\nbody",
    );
    for index in 0..40 {
        skill(
            collection.join(format!("z{index:02}")),
            &format!("skill-{index}"),
            &"猫".repeat(120),
            "body",
        );
    }
    let store = Store::in_memory().unwrap();
    let session = store.create_session("yaml").unwrap().id;
    let skills = registry(&store, root.path(), None);
    skills.check(&session, true, &[]).unwrap();
    let prompt = skills.system_prompt(&session, "");
    assert!(prompt.len() <= MAX_SKILL_BYTES + 2);
    assert!(
        prompt.contains("quoted:name") && prompt.contains("A quoted: description over two lines.")
    );
    assert!(prompt.contains("truncated at 8 KiB"));
    let warnings = skills.take_changes(&session);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Skipped skill"));
    assert!(!warnings[0].contains("[unclosed"));
    skills.check(&session, true, &[]).unwrap();
    assert!(skills.take_changes(&session).is_empty());
}

#[test]
fn long_utf8_body_is_not_loaded_or_confused_with_frontmatter_boundary() {
    let root = tempfile::tempdir().unwrap();
    skill(
        root.path().join(".agents/skills/large"),
        "large",
        "Large body",
        &"猫".repeat(40_000),
    );
    let store = Store::in_memory().unwrap();
    let session = store.create_session("utf8").unwrap().id;
    let skills = registry(&store, root.path(), None);
    skills.check(&session, true, &[]).unwrap();
    assert!(skills.system_prompt(&session, "").contains("Large body"));
    assert!(skills.take_changes(&session).is_empty());
}

#[test]
fn initial_frontmatter_warning_retries_after_interruption_but_not_after_durable_delivery() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path().join(".agents/skills/broken/SKILL.md"),
        "not frontmatter",
    );
    let store = Store::open_data_dir(root.path().join("data")).unwrap();
    let session = store.create_session("warnings").unwrap().id;
    let skills = registry(&store, root.path(), None);
    skills.check(&session, true, &[]).unwrap();
    assert_eq!(skills.take_changes(&session).len(), 1);
    // Simulate interruption before that notice reached the canonical message path.
    drop(skills);
    let skills = registry(&store, root.path(), None);
    skills.check(&session, true, &[]).unwrap();
    assert_eq!(persist(&store, &skills, &session).len(), 1);
    drop(skills);
    let skills = registry(&store, root.path(), None);
    skills.check(&session, true, &[]).unwrap();
    assert!(skills.take_changes(&session).is_empty());
}

#[test]
fn frozen_catalog_live_diff_body_changes_and_reopen_delivery_are_exact_once() {
    let root = tempfile::tempdir().unwrap();
    let collection = root.path().join(".agents/skills");
    skill(
        collection.join("kept"),
        "kept",
        "Kept description",
        "BODY-V1",
    );
    skill(
        collection.join("removed"),
        "removed",
        "Removed description",
        "body",
    );
    let data = root.path().join("data");
    let store = Store::open_data_dir(&data).unwrap();
    let session = store.create_session("freeze").unwrap().id;
    let skills = registry(&store, root.path(), None);
    skills.check(&session, true, &[]).unwrap();
    let frozen = skills.system_prompt(&session, "base");
    skill(
        collection.join("kept"),
        "kept",
        "Kept description",
        "BODY-V2-LONGER",
    );
    std::fs::remove_dir_all(collection.join("removed")).unwrap();
    skill(
        collection.join("added"),
        "added",
        "Added description",
        "body",
    );
    skills.check(&session, true, &[]).unwrap();
    let notices = persist(&store, &skills, &session);
    assert_eq!(notices.len(), 1);
    let delta: Value = serde_json::from_str(notices[0].split_once('\n').unwrap().1).unwrap();
    assert_eq!(delta["added"], json!(["added"]));
    assert_eq!(delta["removed"], json!(["removed"]));
    assert_eq!(delta["changed"], json!(["kept"]));
    assert!(!notices[0].contains("BODY-V2"));
    assert_eq!(skills.system_prompt(&session, "base"), frozen);
    skills.check(&session, true, &[]).unwrap();
    assert!(skills.take_changes(&session).is_empty());
    drop(skills);
    drop(store);
    let store = Store::open_data_dir(&data).unwrap();
    let reopened = registry(&store, root.path(), None);
    reopened.check(&session, true, &[]).unwrap();
    assert_eq!(reopened.system_prompt(&session, "base"), frozen);
    assert!(reopened.take_changes(&session).is_empty());
    assert!(
        reopened
            .read_roots(&session)
            .contains(&collection.join("added").canonicalize().unwrap())
    );
    assert!(
        reopened.read_roots(&session).contains(
            &root
                .path()
                .canonicalize()
                .unwrap()
                .join(".agents/skills/removed")
        )
    );
    skill(
        collection.join("added"),
        "added",
        "Changed while stopped",
        "body",
    );
    drop(reopened);
    let reopened = registry(&store, root.path(), None);
    reopened.check(&session, true, &[]).unwrap();
    assert_eq!(reopened.take_changes(&session).len(), 1);
    assert_eq!(reopened.system_prompt(&session, "base"), frozen);
}

#[test]
fn disabled_initial_absence_stays_frozen_while_enable_and_directory_changes_are_live() {
    let root = tempfile::tempdir().unwrap();
    skill(
        root.path().join(".agents/skills/local"),
        "local",
        "Local skill",
        "body",
    );
    let store = Store::in_memory().unwrap();
    let session = store.create_session("off").unwrap().id;
    let skills = registry(&store, root.path(), None);
    skills.check(&session, false, &[]).unwrap();
    assert_eq!(skills.system_prompt(&session, "base"), "base");
    assert!(skills.take_changes(&session).is_empty());
    skills.check(&session, true, &[]).unwrap();
    assert_eq!(persist(&store, &skills, &session).len(), 1);
    assert_eq!(skills.system_prompt(&session, "base"), "base");
    drop(skills);
    let skills = registry(&store, root.path(), None);
    skills.check(&session, true, &[]).unwrap();
    assert_eq!(skills.system_prompt(&session, "base"), "base");
    assert!(skills.take_changes(&session).is_empty());
    let child = store.create_session("child").unwrap().id;
    skills.check(&child, true, &[]).unwrap();
    assert!(skills.system_prompt(&child, "base").contains("Local skill"));
}

#[test]
fn config_skill_defaults_paths_and_live_classification() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config/config.toml");
    write(&config, "[skills]\ndirs=['relative']\n");
    let options = LoadOptions {
        workspace: root.path().to_owned(),
        home: None,
        custom_file: Some(config),
        env: vec![],
        cli: vec![],
    };
    let mut defaults = options.clone();
    defaults.custom_file = None;
    let defaults = EffectiveConfig::load(defaults).unwrap().settings().unwrap();
    assert!(defaults.skills.enabled);
    assert!(defaults.skills.dirs.is_empty());
    let original = EffectiveConfig::load(options.clone()).unwrap();
    assert!(original.settings().unwrap().skills.enabled);
    assert_eq!(
        original.settings().unwrap().skills.dirs,
        [root.path().join("config/relative")]
    );
    let mut changed = options.clone();
    changed
        .cli
        .push(Override::set("skills.enabled=false").unwrap());
    let changed = EffectiveConfig::load(changed).unwrap();
    assert!(!changed.settings().unwrap().skills.enabled);
    assert!(!changed.restart_needed(&original));
    let published = original
        .settings()
        .unwrap()
        .with_live_provider_settings(changed.settings().unwrap());
    assert!(!published.skills.enabled);
    let mut table_override = options.clone();
    table_override
        .cli
        .push(Override::set("skills={enabled=false,dirs=['cli-skills']}").unwrap());
    let table_settings = EffectiveConfig::load(table_override)
        .unwrap()
        .settings()
        .unwrap();
    assert!(!table_settings.skills.enabled);
    assert_eq!(table_settings.skills.dirs, [root.path().join("cli-skills")]);
    let published = original
        .settings()
        .unwrap()
        .with_live_provider_settings(table_settings);
    assert_eq!(published.skills.dirs, [root.path().join("cli-skills")]);
    for invalid in [
        "skills.typo=true",
        "skills.enabled='true'",
        "skills.dirs='directory'",
    ] {
        let mut invalid_options = options.clone();
        invalid_options.cli.push(Override::set(invalid).unwrap());
        assert!(
            EffectiveConfig::load(invalid_options).is_err(),
            "accepted {invalid}"
        );
    }
}

fn context(session: &str) -> ToolContext {
    ToolContext {
        session_id: session.into(),
        run_id: "run".into(),
        invocation_id: "invoke".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}

#[tokio::test]
async fn external_listed_folders_allow_only_canonical_reads_and_only_for_the_session() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let external = root.path().join("external");
    skill(
        external.join("listed"),
        "listed",
        "Listed external",
        "EXTERNAL-SKILL-BODY",
    );
    write(external.join("listed/helpers/example.txt"), "HELPER");
    write(external.join("unlisted/private.txt"), "SIBLING");
    let store = Store::in_memory().unwrap();
    let session = store.create_session("allowed").unwrap().id;
    let other = store.create_session("other").unwrap().id;
    let skills = registry(&store, &workspace, Some(external.clone()));
    skills.check(&session, true, &[]).unwrap();
    let files = WorkspaceTools::new(&workspace).unwrap();
    files
        .set_skill_read_roots(&session, skills.read_roots(&session))
        .unwrap();
    let mut tools = ToolRegistry::new();
    files.register(&mut tools, false).unwrap();
    let read = tools.get_host("read").unwrap();
    let body = read
        .invoke(
            context(&session),
            json!({"path":external.join("listed/SKILL.md")}),
        )
        .await
        .unwrap();
    assert!(!body.is_error && body.content.contains("EXTERNAL-SKILL-BODY"));
    let helper = read
        .invoke(
            context(&session),
            json!({"path":external.join("listed/helpers/example.txt")}),
        )
        .await
        .unwrap();
    assert!(!helper.is_error && helper.content.contains("HELPER"));
    for (session, path) in [
        (&other, external.join("listed/SKILL.md")),
        (&session, external.join("unlisted/private.txt")),
    ] {
        let result = read.invoke(context(session), json!({"path":path})).await;
        assert!(result.is_err() || result.unwrap().is_error);
    }
    for (name, input) in [
        ("list", json!({"path":external.join("listed")})),
        (
            "search",
            json!({"path":external.join("listed"),"query":"HELPER"}),
        ),
        (
            "write",
            json!({"path":external.join("listed/SKILL.md"),"content":"blocked overwrite"}),
        ),
    ] {
        let result = tools
            .get_host(name)
            .unwrap()
            .invoke(context(&session), input)
            .await;
        assert!(
            result.is_err() || result.unwrap().is_error,
            "{name} widened beyond workspace"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(external.join("unlisted"), external.join("listed/escape"))
            .unwrap();
        let result = read
            .invoke(
                context(&session),
                json!({"path":external.join("listed/escape/private.txt")}),
            )
            .await;
        assert!(result.is_err() || result.unwrap().is_error);

        // A saved canonical grant must keep its original identity when an ancestor
        // later becomes a symlink. Refreshing that grant cannot authorize the target.
        let replacement = root.path().join("replacement");
        write(replacement.join("listed/secret.txt"), "REPLACEMENT-SECRET");
        std::fs::rename(&external, root.path().join("old-external")).unwrap();
        std::os::unix::fs::symlink(&replacement, &external).unwrap();
        files
            .set_skill_read_roots(&session, skills.read_roots(&session))
            .unwrap();
        let result = read
            .invoke(
                context(&session),
                json!({"path":external.join("listed/secret.txt")}),
            )
            .await;
        assert!(result.is_err() || result.unwrap().is_error);
    }
}
