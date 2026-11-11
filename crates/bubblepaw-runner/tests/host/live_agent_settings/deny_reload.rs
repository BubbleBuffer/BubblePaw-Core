use super::*;

#[tokio::test]
async fn running_child_resolves_deny_additions_and_removals_through_parent_override() -> Result<()>
{
    for initially_denied in [false, true] {
        let mut fixture = Fixture::new(None)?;
        if initially_denied {
            fixture.policy.write().unwrap().deny.push("effect".into());
        }
        fixture
            .parent
            .prompt(
                &fixture.owner.session_id,
                "spawn with pinned allow",
                RunOptions {
                    script: Some(
                        "return await tools.agent({task:'gated effect',profile:'workspace'});"
                            .into(),
                    ),
                    approval: Some(allow()),
                    ..Default::default()
                },
            )
            .await?;
        let child = tokio::time::timeout(Duration::from_secs(5), fixture.entered.recv())
            .await?
            .unwrap();
        if initially_denied {
            fixture.policy.write().unwrap().deny.clear();
        } else {
            fixture.policy.write().unwrap().deny.push("effect".into());
        }
        fixture.probe.release.notify_one();
        let result = fixture
            .manager
            .wait(&fixture.owner.session_id, &child, Some(5000))
            .await?;
        assert_eq!(result["status"], "completed");
        assert_eq!(
            fixture.effects.load(Ordering::SeqCst),
            usize::from(initially_denied)
        );
        let denied = fixture.store.messages(&child)?.iter().any(|message| {
            message.role == Role::Tool
                && message.is_error
                && message.content.contains("deny rule effect")
        });
        assert_eq!(denied, !initially_denied);
        fixture.manager.shutdown().await?;
    }
    Ok(())
}
