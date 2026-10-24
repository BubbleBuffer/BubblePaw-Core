use super::*;

#[tokio::test]
async fn crawl_token_uses_invocation_snapshot_across_backend_reload() {
    const CHILD: &str = "BP_LIVE_ROLES_NET_TOKEN_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crawl_token_uses_invocation_snapshot_across_backend_reload",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CRAWL4AI_API_TOKEN", "old-fixture-token")
            .status()
            .unwrap();
        assert!(status.success(), "isolated token fixture failed");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (old_base, old_fixture, old_server) = serve("old", false, false).await;
    let (new_base, new_fixture, new_server) = serve("new", false, false).await;
    let live = Arc::new(
        LiveProviderConfig::new(configuration(root.path(), &old_base, "old", "")).unwrap(),
    );
    let web = web_configuration(&live);
    let client =
        NetClient::with_store_live(&web, &Store::in_memory().unwrap(), live.clone()).unwrap();
    // This isolated subprocess can change its host environment without racing
    // other tests. Neither an in-flight generation nor a prepared reload reads it.
    unsafe {
        std::env::set_var("CRAWL4AI_API_TOKEN", "new-fixture-token");
    }
    client
        .fetch("https://example.com/old-token", 0, 1000)
        .await
        .unwrap();
    assert_eq!(*old_fixture.crawl_authenticated.lock().unwrap(), [true]);
    apply(&live, configuration(root.path(), &new_base, "new", ""));
    unsafe {
        std::env::set_var("CRAWL4AI_API_TOKEN", "changed-after-apply-fixture-token");
    }
    client
        .fetch("https://example.com/new-token", 0, 1000)
        .await
        .unwrap();
    assert_eq!(*new_fixture.crawl_authenticated.lock().unwrap(), [true]);
    let debug = format!("{:?}", live.snapshot().net);
    assert!(!debug.contains("invocation-fixture-token"));
    let before = live.snapshot();
    unsafe {
        std::env::set_var("CRAWL4AI_API_TOKEN", "invalid\nfixture-private-token");
    }
    // Even an invalid host token cannot affect a candidate built from the
    // invocation snapshot; preparing it does not publish a new generation.
    assert!(
        live.prepare(configuration(root.path(), &new_base, "new", ""))
            .is_ok()
    );
    assert!(Arc::ptr_eq(&before, &live.snapshot()));
    // Invalid credentials supplied by a candidate snapshot still fail closed.
    let error = live
        .prepare(configuration_with_token(
            root.path(),
            &new_base,
            "new",
            "",
            "invalid\nfixture-private-token",
        ))
        .err()
        .expect("invalid token header must fail preparation");
    assert_eq!(error.to_string(), "invalid Crawl4AI API token header");
    assert!(Arc::ptr_eq(&before, &live.snapshot()));
    old_server.abort();
    new_server.abort();
}
