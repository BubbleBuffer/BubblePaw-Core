//! Web settings, backends and validation.
use super::*;

#[test]
fn web_defaults_and_layering_preserve_sources_without_reading_keys() {
    let root = tempfile::tempdir().unwrap();
    let defaults = EffectiveConfig::load(options(root.path())).unwrap();
    let web = defaults.settings().unwrap().web;
    assert!(!web.enabled);
    assert!(web.expose.fetch && web.expose.search);
    assert_eq!(web.search, vec!["deepseek", "ddg"]);
    assert_eq!(web.deepseek_key_env, "DEEPSEEK_API_KEY");
    assert_eq!(web.deepseek_base_url, crate::net::DEEPSEEK_SEARCH_BASE_URL);
    assert_eq!(web.fetch_backend, vec!["direct"]);
    assert_eq!(web.cooldown_secs, 300);
    assert_eq!(
        (
            web.cache_ttl_secs,
            web.cache_max_bytes,
            web.max_searches,
            web.max_fetches
        ),
        (86400, 64 * 1024 * 1024, None, None)
    );
    assert!(web.net_config().unwrap().is_none());
    let path = root.path().join("web.toml");
    std::fs::write(
        &path,
        r#"
[web]
enabled = true
search = "brave"
key_env = "BUBBLEPAW_SYNTHETIC_BRAVE_SECRET"
max_searches = 3
cache_max_bytes = 1048576
"#,
    )
    .unwrap();
    let mut opts = options(root.path());
    opts.custom_file = Some(path.clone());
    opts.env = vec![
        (
            "BUBBLEPAW_SYNTHETIC_BRAVE_SECRET".into(),
            "never-render-this-value".into(),
        ),
        ("BUBBLEPAW_WEB__MAX_SEARCHES".into(), "4".into()),
        ("BUBBLEPAW_WEB__EXPOSE__SEARCH".into(), "true".into()),
    ];
    opts.cli = vec![Override::set("web.max_searches=5").unwrap()];
    // Loading/rendering enabled Brave succeeds even with no real key in the process.
    let effective = EffectiveConfig::load(opts).unwrap();
    let web = effective.settings().unwrap().web;
    assert!(web.enabled && web.expose.search);
    assert_eq!(web.max_searches, Some(5));
    assert_eq!(web.max_fetches, None);
    assert_eq!(web.cache_max_bytes, 1048576);
    assert_eq!(
        effective.source("web.cache_max_bytes"),
        Some(format!("custom: {}", path.display()).as_str())
    );
    assert_eq!(effective.source("web.max_searches"), Some("CLI"));
    assert_eq!(effective.source("web.expose.search"), Some("env"));
    let rendered = effective.render().unwrap();
    assert!(rendered.contains("BUBBLEPAW_SYNTHETIC_BRAVE_SECRET"));
    assert!(!rendered.contains("never-render-this-value"));
}

#[test]
fn web_config_validates_disabled_settings_and_rejects_credentials() {
    let root = tempfile::tempdir().unwrap();
    for entry in [
        "web.search=invalid",
        "web.search=searxng:file:///tmp/search",
        "web.search=searxng:https://user:secret@example.org",
        "web.fetch_backend=fancy",
        "web.fetch_backend=crawl4ai:",
        "web.fetch_backend=crawl4ai:file:///tmp/crawl",
        "web.fetch_backend=crawl4ai:https://user:secret@example.org",
        "web.search=[]",
        "web.fetch_backend=[]",
        "web.search=[\"ddg\", \"invalid\"]",
        "web.fetch_backend=[\"direct\", \"fancy\"]",
        "web.cooldown_secs=-1",
        "web.max_searches=0",
        "web.max_fetches=0",
        "web.cache_ttl_secs=-1",
        "web.cache_max_bytes=-1",
        "web.research.quick_rounds=6",
        "web.research.model='demo/research'",
        "web.expose.research=true",
        "web.api_key=secret",
        "web.deepseek_api_key=secret",
        "web.deepseek_key_env=''",
        "web.deepseek_base_url=file:///tmp/search",
        "web.deepseek_base_url=https://user:secret@example.org",
        "web.expose.unknown=true",
    ] {
        let mut opts = options(root.path());
        opts.cli = vec![Override::set(entry).unwrap()];
        assert!(EffectiveConfig::load(opts).is_err(), "{entry}");
    }
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("web.enabled=true").unwrap(),
        Override::set("web.cache_ttl_secs=0").unwrap(),
    ];
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    let config = settings.web.net_config().unwrap().unwrap();
    assert_eq!(config.cache_ttl_secs, 0);
    assert!(matches!(
        config.search.as_slice(),
        [SearchBackend::DeepSeek { base_url, .. }, SearchBackend::DuckDuckGo]
            if base_url == crate::net::DEEPSEEK_SEARCH_BASE_URL
    ));
    assert_eq!(config.fetch, vec![FetchBackend::Direct]);
}

#[test]
fn web_backend_lists_accept_a_string_or_array_in_every_layer() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("web.toml");
    write(
        &path,
        "[web]\nenabled = true\nsearch = [\"searxng:http://127.0.0.1:8888\", \"ddg\"]\n",
    );
    let mut opts = options(root.path());
    opts.custom_file = Some(path);
    opts.env = vec![(
        "BUBBLEPAW_WEB__FETCH_BACKEND".into(),
        "[\"crawl4ai:http://127.0.0.1:11235\", \"direct\"]".into(),
    )];
    opts.cli = vec![Override::set("web.cooldown_secs=60").unwrap()];
    let effective = EffectiveConfig::load(opts).unwrap();
    let settings = effective.settings().unwrap();
    assert_eq!(
        settings.web.search,
        vec!["searxng:http://127.0.0.1:8888", "ddg"]
    );
    assert_eq!(
        settings.web.fetch_backend,
        vec!["crawl4ai:http://127.0.0.1:11235", "direct"]
    );
    let config = settings.web.net_config().unwrap().unwrap();
    assert_eq!(config.search.len(), 2);
    assert_eq!(config.fetch.len(), 2);
    assert_eq!(config.cooldown_secs, 60);
    assert!(
        effective
            .source("web.search")
            .is_some_and(|source| source.starts_with("custom"))
    );
    assert!(effective.render().unwrap().contains("cooldown_secs"));

    // --set accepts the same array syntax, and a bare string stays one entry.
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("web.enabled=true").unwrap(),
        Override::set("web.search=[\"brave\", \"ddg\"]").unwrap(),
        Override::set("web.fetch_backend=direct").unwrap(),
    ];
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    assert_eq!(settings.web.search, vec!["brave", "ddg"]);
    assert_eq!(settings.web.fetch_backend, vec!["direct"]);
}

#[test]
fn web_deepseek_settings_layer_reload_and_missing_key() {
    let root = tempfile::tempdir().unwrap();
    let initial = EffectiveConfig::load(options(root.path())).unwrap();
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("web.enabled=true").unwrap(),
        Override::set("web.deepseek_key_env=BUBBLEPAW_TEST_UNSET_DEEPSEEK_KEY").unwrap(),
        Override::set("web.deepseek_base_url=http://127.0.0.1:18081/v1/tools").unwrap(),
    ];
    let candidate = EffectiveConfig::load(opts).unwrap();
    assert!(!candidate.restart_needed(&initial));
    let projected = initial.with_live_from(&candidate).settings().unwrap();
    assert_eq!(
        projected.web.deepseek_key_env,
        "BUBBLEPAW_TEST_UNSET_DEEPSEEK_KEY"
    );
    let config = projected.web.net_config().unwrap().unwrap();
    assert!(matches!(config.search.as_slice(),
        [SearchBackend::DeepSeek { base_url, api_key }, SearchBackend::DuckDuckGo]
            if base_url == "http://127.0.0.1:18081/v1/tools" && api_key.is_empty()));
}

#[test]
fn brave_without_a_key_is_not_a_config_error() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("web.enabled=true").unwrap(),
        Override::set("web.search=[\"brave\", \"ddg\"]").unwrap(),
        Override::set("web.key_env=BUBBLEPAW_TEST_UNSET_BRAVE_KEY").unwrap(),
    ];
    let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
    let config = settings.web.net_config().unwrap().unwrap();
    assert!(matches!(
        config.search.as_slice(),
        [SearchBackend::Brave { api_key }, SearchBackend::DuckDuckGo] if api_key.is_empty()
    ));
}

#[test]
fn web_fetch_backend_parses_crawl4ai_base_url() {
    let root = tempfile::tempdir().unwrap();
    let mut opts = options(root.path());
    opts.cli = vec![
        Override::set("web.enabled=true").unwrap(),
        Override::set("web.fetch_backend=crawl4ai:http://127.0.0.1:11235").unwrap(),
    ];
    let effective = EffectiveConfig::load(opts).unwrap();
    assert_eq!(effective.source("web.fetch_backend"), Some("CLI"));
    let settings = effective.settings().unwrap();
    let config = settings.web.net_config().unwrap().unwrap();
    assert_eq!(
        config.fetch,
        vec![FetchBackend::Crawl4Ai {
            url: "http://127.0.0.1:11235".into()
        }]
    );
    assert!(effective.render().unwrap().contains("fetch_backend"));
}
