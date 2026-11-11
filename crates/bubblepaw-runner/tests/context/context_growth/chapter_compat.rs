//! Chapter metadata compatibility keeps the existing test coverage unchanged.
use super::*;

#[tokio::test]
async fn ten_explicit_large_outputs_are_summarized_without_masking_or_replaying_effects()
-> Result<()> {
    let f = Fixture::new(256000, Some(272000))?;
    let original = f.seed("x ".repeat(247000))?;
    let cells: Vec<_> = (0..10)
        .map(|index| format!("text('growth-action-{index}'); return 'x '.repeat(30000);"))
        .collect();
    f.provider
        .schedule_with_limit(&cells.iter().map(String::as_str).collect::<Vec<_>>(), 32000);
    let latest = "Retain this exact current user objective after all ten actions.";
    let run = f.prompt(latest).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let archive = f.store.messages(&f.session)?;
    assert_eq!(
        serde_json::to_value(&archive[..original.len()])?,
        serde_json::to_value(&original)?
    );
    let tools = archive
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 10);
    assert!(
        tools
            .iter()
            .all(|m| !m.is_error && m.content.len() == 60002)
    );
    f.assert_summarized_continuation(
        &original,
        latest,
        &vec!["x ".repeat(30000); 10],
        256000,
        true,
    )?;
    Ok(())
}
