use super::*;

#[tokio::test]
async fn manual_compact_bypasses_disabled_auto_trigger_and_keeps_originals() -> Result<()> {
    for focus in [None, Some("Keep exact paths and unresolved work")] {
        let f = Fixture::new(ContextStrategy::Lcm)?;
        f.turns(3)?;
        let history = f.store.messages(&f.session)?;
        let before = f.store.storage_stats()?;
        let notice = f.harness.compact_session(&f.session, focus).await?.unwrap();
        assert!(notice.contains("Context summarized"));
        assert_eq!(f.store.messages(&f.session)?, history);
        let view = f.store.context_view(&f.session)?.unwrap();
        assert_eq!(
            view.frontier,
            Some(f.store.message_seq(&f.session, &history[3].id)?)
        );
        assert!(view.system_prompt.is_empty());
        assert_eq!(f.models.inputs.lock().unwrap().len(), 1);
        assert_eq!(f.models.inputs.lock().unwrap()[0].focus.as_deref(), focus);
        let after = f.store.storage_stats()?;
        println!(
            "manual compact focus={focus:?}: DB {} -> {} bytes; WAL {} -> {} bytes; transactions {} -> {} (delta {}); canonical history unchanged",
            before.database_bytes,
            after.database_bytes,
            before.wal_bytes,
            after.wal_bytes,
            before.transaction_count,
            after.transaction_count,
            after.transaction_count - before.transaction_count
        );
        assert_eq!(after.message_count, before.message_count);
        // First compaction binds the chat prefix, saves the episode batch,
        // writes the encoded view in the chat store and host projection, then
        // persists the applied tool catalog. Repeated no-ops write nothing.
        assert_eq!(after.transaction_count - before.transaction_count, 5);
        // No new older span means no repeated model request or store writes.
        let notice = f.harness.compact_session(&f.session, None).await?.unwrap();
        assert!(notice.contains("Nothing to compact"));
        assert_eq!(
            f.store.storage_stats()?.transaction_count,
            after.transaction_count
        );
        let prepared = prepare_fresh(
            &*f.context,
            Arc::new(NoMain),
            &f.session,
            ModelRequest {
                model: "scripted".into(),
                system_prompt: "Keep original instructions".into(),
                messages: history,
                ..Default::default()
            },
        )
        .await?;
        assert!(
            prepared
                .request
                .messages
                .iter()
                .any(|m| m.content == "Original decisions retained."
                    || m.content.contains("Original decisions retained."))
        );
        assert!(
            prepared
                .request
                .messages
                .iter()
                .any(|m| m.content == "Exact request 2")
        );
    }
    Ok(())
}

#[tokio::test]
async fn manual_compact_short_history_is_a_noop_and_classical_is_rejected() -> Result<()> {
    let f = Fixture::new(ContextStrategy::Lcm)?;
    for count in [0, 1] {
        f.turns(count)?;
        let before = f.store.storage_stats()?.transaction_count;
        assert!(
            f.harness
                .compact_session(&f.session, None)
                .await?
                .unwrap()
                .contains("Nothing to compact")
        );
        // The first empty compact binds the immutable chat prefix once;
        // the next short-history call already has that binding.
        let after = f.store.storage_stats()?.transaction_count;
        assert_eq!(after - before, u64::from(count == 0));
        assert!(
            f.harness
                .compact_session(&f.session, None)
                .await?
                .unwrap()
                .contains("Nothing to compact")
        );
        assert_eq!(f.store.storage_stats()?.transaction_count, after);
        assert!(f.store.context_view(&f.session)?.is_none());
        assert!(f.models.inputs.lock().unwrap().is_empty());
    }
    let classical = Fixture::new(ContextStrategy::Classical)?;
    assert!(
        classical
            .harness
            .compact_session(&classical.session, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("requires an LCM session")
    );
    Ok(())
}
