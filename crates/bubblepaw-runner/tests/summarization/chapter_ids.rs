use super::*;

#[tokio::test]
async fn chapter_ids_are_frozen_with_summary_text_across_later_turns() -> Result<()> {
    let fixture = Fixture::new()?;
    let history = fixture.turns(8, 35000);
    fixture.seed(&history)?;
    fixture.save_episodes(&history, 7, 6000, true)?;
    let policy = fixture.policy(Script::normal(), 256000)?;
    let first = fixture.prepare(&policy).await?;
    let view = fixture.store.context_view(&fixture.session)?.unwrap();
    let summary = view
        .prefix
        .iter()
        .find(|message| message.content.starts_with("Closed chapter summaries"))
        .unwrap();
    assert!(summary.content.contains("[ch-1]\nFUTURE_CHAPTER_SECRET"));
    assert_eq!(summary.content.matches("[ch-1]").count(), 1);
    assert!(
        view.prefix
            .iter()
            .any(|message| message.content.contains("[ch-1] Chapter 1"))
    );
    let frozen = serde_json::to_vec(&view.prefix)?;
    let declarations = serde_json::to_vec(&first.request.tools)?;
    fixture.seed(&[
        fixture.message(Role::User, "Continue with a new task."),
        fixture.message(Role::Assistant, "The next task is complete."),
    ])?;
    let later = fixture.prepare(&policy).await?;
    let later_view = fixture.store.context_view(&fixture.session)?.unwrap();
    assert_eq!(frozen, serde_json::to_vec(&later_view.prefix)?);
    assert_eq!(declarations, serde_json::to_vec(&later.request.tools)?);
    assert!(
        later
            .request
            .messages
            .iter()
            .any(|message| message == summary)
    );
    Ok(())
}
