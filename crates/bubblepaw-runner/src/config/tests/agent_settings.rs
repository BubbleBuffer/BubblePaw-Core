//! Agent limits, defaults and reminder settings.
use super::*;

#[test]
fn agents_defaults_and_layers_preserve_sources() {
    let root = tempfile::tempdir().unwrap();
    let defaults = EffectiveConfig::load(options(root.path())).unwrap();
    assert_eq!(
        defaults.settings().unwrap().agents,
        Agents {
            max_depth: None,
            max_active_per_parent: None,
            max_active_total: None,
            max_retained: None,
            stale_reminder_secs: 7200,
            auto_wake: true,
            wake_debounce_ms: 1500,
            worker_retention_days: 7,
            worker_keep_all: false,
        }
    );
    let rendered: Value = toml::from_str(&defaults.render().unwrap()).unwrap();
    assert_eq!(
        rendered["agents"]["stale_reminder_secs"].as_integer(),
        Some(7200)
    );
    assert_eq!(
        defaults.source("agents.stale_reminder_secs"),
        Some("default")
    );
    assert_eq!(
        rendered["agents"]["worker_retention_days"].as_integer(),
        Some(7)
    );
    assert_eq!(rendered["agents"]["worker_keep_all"].as_bool(), Some(false));
    assert_eq!(
        defaults.source("agents.worker_retention_days"),
        Some("default")
    );
    assert_eq!(defaults.source("agents.worker_keep_all"), Some("default"));
    for key in [
        "max_depth",
        "max_active_per_parent",
        "max_active_total",
        "max_retained",
    ] {
        assert!(
            rendered["agents"].get(key).is_none(),
            "default cap {key} is set"
        );
    }
    let path = root.path().join("agents.toml");
    std::fs::write(
        &path,
        "[agents]\nmax_depth=3\nmax_active_per_parent=5\nmax_active_total=8\nmax_retained=16\nworker_retention_days=9\nworker_keep_all=true\n",
    )
    .unwrap();
    let mut opts = options(root.path());
    opts.custom_file = Some(path);
    let file = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(file.settings().unwrap().agents.worker_retention_days, 9);
    assert!(file.settings().unwrap().agents.worker_keep_all);
    for key in ["agents.worker_retention_days", "agents.worker_keep_all"] {
        assert!(file.source(key).unwrap().contains("agents.toml"));
    }
    opts.env = vec![
        ("BUBBLEPAW_AGENTS__MAX_DEPTH".into(), "2".into()),
        ("BUBBLEPAW_AGENTS__MAX_ACTIVE_TOTAL".into(), "10".into()),
        ("BUBBLEPAW_AGENTS__WORKER_RETENTION_DAYS".into(), "3".into()),
        ("BUBBLEPAW_AGENTS__WORKER_KEEP_ALL".into(), "false".into()),
    ];
    let env = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(env.settings().unwrap().agents.worker_retention_days, 3);
    assert!(!env.settings().unwrap().agents.worker_keep_all);
    assert_eq!(env.source("agents.worker_retention_days"), Some("env"));
    assert_eq!(env.source("agents.worker_keep_all"), Some("env"));
    opts.cli = vec![
        Override::set("agents.max_depth=0").unwrap(),
        Override::set("agents.worker_retention_days=0").unwrap(),
        Override::set("agents.worker_keep_all=true").unwrap(),
    ];
    let effective = EffectiveConfig::load(opts).unwrap();
    assert_eq!(
        effective.settings().unwrap().agents,
        Agents {
            max_depth: Some(0),
            max_active_per_parent: Some(5),
            max_active_total: Some(10),
            max_retained: Some(16),
            stale_reminder_secs: 7200,
            auto_wake: true,
            wake_debounce_ms: 1500,
            worker_retention_days: 0,
            worker_keep_all: true,
        }
    );
    assert_eq!(effective.source("agents.max_depth"), Some("CLI"));
    assert_eq!(effective.source("agents.max_active_total"), Some("env"));
    assert_eq!(
        effective.source("agents.worker_retention_days"),
        Some("CLI")
    );
    assert_eq!(effective.source("agents.worker_keep_all"), Some("CLI"));
    let rendered: Value = toml::from_str(&effective.render().unwrap()).unwrap();
    assert_eq!(rendered["agents"]["max_depth"].as_integer(), Some(0));
    assert_eq!(
        rendered["agents"]["worker_retention_days"].as_integer(),
        Some(0)
    );
    assert_eq!(rendered["agents"]["worker_keep_all"].as_bool(), Some(true));
    assert_eq!(
        rendered["agents"]["max_active_per_parent"].as_integer(),
        Some(5)
    );
}

#[test]
fn agents_limits_validate_values_and_unknown_keys() {
    let root = tempfile::tempdir().unwrap();
    for entry in [
        "agents.max_active_per_parent=0",
        "agents.max_active_total=0",
        "agents.max_retained=0",
        "agents.max_depth=-1",
        "agents.max_active_total=-1",
        "agents.max_depth=1.5",
        "agents.max_retained='many'",
        "agents.stale_reminder_secs=-1",
        "agents.stale_reminder_secs=1.5",
        "agents.stale_reminder_secs='soon'",
        "agents.worker_retention_days=-1",
        "agents.worker_retention_days=1.5",
        "agents.worker_retention_days='soon'",
        "agents.worker_retention_days=4294967296",
        "agents.worker_keep_all=1",
        "agents.worker_keep_all='yes'",
        "agents.unknown=1",
    ] {
        let mut opts = options(root.path());
        opts.cli = vec![Override::set(entry).unwrap()];
        assert!(EffectiveConfig::load(opts).is_err(), "accepted {entry}");
    }
    let mut opts = options(root.path());
    opts.cli = vec![Override::set("agents.max_retained=1").unwrap()];
    assert_eq!(
        EffectiveConfig::load(opts)
            .unwrap()
            .settings()
            .unwrap()
            .agents
            .max_retained,
        Some(1)
    );
}

#[test]
fn agents_reminder_interval_layers_and_can_be_disabled() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("reminders.toml");
    std::fs::write(&path, "[agents]\nstale_reminder_secs=15\n").unwrap();
    let mut opts = options(root.path());
    opts.custom_file = Some(path);
    opts.env = vec![("BUBBLEPAW_AGENTS__STALE_REMINDER_SECS".into(), "3".into())];
    let effective = EffectiveConfig::load(opts.clone()).unwrap();
    assert_eq!(effective.settings().unwrap().agents.stale_reminder_secs, 3);
    assert_eq!(effective.source("agents.stale_reminder_secs"), Some("env"));
    opts.cli = vec![Override::set("agents.stale_reminder_secs=0").unwrap()];
    let disabled = EffectiveConfig::load(opts).unwrap();
    assert_eq!(disabled.settings().unwrap().agents.stale_reminder_secs, 0);
    assert_eq!(disabled.source("agents.stale_reminder_secs"), Some("CLI"));
}
