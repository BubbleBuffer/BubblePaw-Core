//! `[exec]` defaults, layering, validation and rendering (docs/exec.md, docs/config.md).
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

#[test]
fn exec_defaults_leave_job_caps_unset_and_keep_a_five_minute_sync_window() {
    let root = tempfile::tempdir().unwrap();
    let effective = EffectiveConfig::load(options(root.path())).unwrap();
    let exec = effective.settings().unwrap().exec;
    assert_eq!(exec.sync_timeout_secs, 300);
    assert_eq!(exec.max_jobs_per_session, None);
    assert_eq!(exec.max_jobs, None);
    assert_eq!(exec.max_output_bytes, 8 * 1024 * 1024);
    assert_eq!(effective.source("exec.sync_timeout_secs"), Some("default"));
    let built_in = ExecConfig::default();
    assert_eq!(built_in.sync_timeout_secs, 300);
    assert_eq!(built_in.max_jobs, None);
    assert_eq!(built_in.max_output_bytes, 8 * 1024 * 1024);
}

#[test]
fn exec_layers_file_env_and_cli_with_their_sources() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("exec.toml");
    std::fs::write(&path, "[exec]\nsync_timeout_secs = 5\nmax_jobs = 3\n").unwrap();
    let mut opts = options(root.path());
    opts.custom_file = Some(path);
    opts.env = vec![
        ("BUBBLEPAW_EXEC__MAX_JOBS_PER_SESSION".into(), "2".into()),
        ("BUBBLEPAW_EXEC__MAX_OUTPUT_BYTES".into(), "1024".into()),
    ];
    // CLI wins over the file, and 0 is a valid sync window: background at once.
    opts.cli = vec![Override::set("exec.sync_timeout_secs=0").unwrap()];
    let effective = EffectiveConfig::load(opts).unwrap();
    let exec = effective.settings().unwrap().exec;
    assert_eq!(exec.sync_timeout_secs, 0);
    assert_eq!(exec.max_jobs, Some(3));
    assert_eq!(exec.max_jobs_per_session, Some(2));
    assert_eq!(exec.max_output_bytes, 1024);
    assert_eq!(effective.source("exec.sync_timeout_secs"), Some("CLI"));
    assert!(
        effective
            .source("exec.max_jobs")
            .is_some_and(|source| source.starts_with("custom"))
    );
    assert_eq!(effective.source("exec.max_jobs_per_session"), Some("env"));
    let rendered = effective.render().unwrap();
    assert!(rendered.contains("max_jobs_per_session"));
}

#[test]
fn exec_rejects_unknown_keys_bad_types_and_non_positive_caps() {
    let root = tempfile::tempdir().unwrap();
    for entry in [
        "exec.typo=1",
        "exec.sync_timeout_secs=-1",
        "exec.max_jobs=0",
        "exec.max_jobs_per_session=0",
        "exec.max_output_bytes=0",
    ] {
        let mut opts = options(root.path());
        opts.cli = vec![Override::set(entry).unwrap()];
        assert!(
            EffectiveConfig::load(opts).is_err(),
            "{entry} should be rejected"
        );
    }
}
