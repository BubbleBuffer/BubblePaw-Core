use super::*;

#[tokio::test]
#[ignore = "explicitly gated paid claude-haiku-5-5 Messages capture"]
async fn live_research_child() -> Result<()> {
    if std::env::var("BP_LIVE_RESEARCH_400").ok().as_deref() != Some("1") {
        println!("skipped: BP_LIVE_RESEARCH_400=1 not set");
        return Ok(());
    }
    let key = std::env::var("OPENCODE_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .context("OPENCODE_API_KEY not set")?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.run/live400");
    let counter = std::env::var_os("BP_RESEARCH_LIVE400_COUNTER")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("request-count"));
    let directory = root.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&directory)?;
    println!("live400 capture: {}", directory.display());
    let upstream = std::env::var("BP_LIVE_OPENCODE_MESSAGES_BASE_URL")
        .unwrap_or_else(|_| "https://opencode.ai/zen/go/v1".into());
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()?;
    run_research_child(
        Transport::Live {
            upstream,
            client,
            directory,
            counter,
        },
        key,
        ProbeCase::from_env()?,
    )
    .await
}
