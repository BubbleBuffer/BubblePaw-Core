use super::*;

#[tokio::test]
async fn small_window_serves_summaries_and_preserves_latest_large_group_without_replay()
-> Result<()> {
    let f = Fixture::new(32000, Some(48000))?;
    let original = f.seed("x ".repeat(26000))?;
    f.provider.schedule_with_limit(
        &["text('growth-action-0'); return 'x '.repeat(28000);"],
        30000,
    );
    let latest = "Keep this current user instruction literal.";
    let run = f.prompt(latest).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let archive = f.store.messages(&f.session)?;
    let tools = archive
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 1);
    assert!(!tools[0].is_error);
    assert_eq!(tools[0].content.len(), 56002);
    f.assert_summarized_continuation(&original, latest, &["x ".repeat(28000)], 32000, false)?;
    Ok(())
}
