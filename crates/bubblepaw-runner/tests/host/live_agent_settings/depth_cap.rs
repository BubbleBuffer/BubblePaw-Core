use super::*;

#[tokio::test]
async fn default_depth_cap_refuses_spawn_from_depth_three_only() -> Result<()> {
    let fixture = Fixture::new(Agents::default().max_depth)?;
    let mut owner = fixture.owner.clone();
    // Depth 1 and 2 agents (and the root) may spawn; the depth-3 agent may not.
    for _ in 0..3 {
        let child = fixture.manager.start(&owner, json!({"task":"nest"}))?;
        fixture.settle(&owner.session_id, &child).await?;
        owner = ToolContext {
            session_id: child["sessionId"].as_str().unwrap().into(),
            ..fixture.owner.clone()
        };
    }
    let refused = fixture
        .manager
        .start(&owner, json!({"task":"too deep"}))
        .unwrap_err()
        .to_string();
    assert_eq!(refused, "subagent depth limit 3 reached");
    fixture.manager.shutdown().await?;
    Ok(())
}
