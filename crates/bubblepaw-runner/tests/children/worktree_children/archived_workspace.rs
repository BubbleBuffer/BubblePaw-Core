use super::*;

#[tokio::test]
async fn inherited_grandchild_send_restores_its_archived_parent_workspace() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repo = directory.path().join("repo");
    initialize(&repo)?;
    let store = Store::open_data_dir(directory.path().join("data"))?;
    let owner = store.create_session("parent")?.id;
    let host = Host::new(&store, &repo, Arc::new(Script::default()))?;
    let spawn = format!(
        "return await tools.agent({});",
        json!({"task":task("return await tools.read({path:'base.txt'});"),"profile":"workspace","worktree":false})
    );
    let child = host.agents.start(
        &context(&owner),
        json!({"task":task(&spawn),"profile":"workspace","worktree":true}),
    )?;
    let child_id = child["sessionId"].as_str().unwrap();
    host.wait(&owner, child_id).await?;
    let grandchild = store
        .list_sessions()?
        .into_iter()
        .find(|session| session.parent_session_id.as_deref() == Some(child_id))
        .unwrap();
    host.wait(child_id, &grandchild.id).await?;
    let info = worktrees::get(&store, child_id)?.unwrap();
    assert!(worktrees::get(&store, &grandchild.id)?.is_none());
    let (archived, reports) = worktrees::archive_chat(&store, &owner)?;
    assert!(archived);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].status, worktrees::CleanupStatus::Removed);
    assert!(!info.path.exists());
    assert!(store.active_session_metadata(&grandchild.id)?.is_none());
    let resumed = format!(
        "const prior=await tools.read({{path:'base.txt'}}); {} return prior;",
        write_file("inherited-restored.txt", "restored inherited workspace")
    );
    host.agents.send(
        &context(child_id),
        json!({"sessionId":grandchild.id,"text":task(&resumed)}),
    )?;
    host.wait(child_id, &grandchild.id).await?;
    assert_eq!(
        std::fs::read_to_string(info.path.join("inherited-restored.txt"))?,
        "restored inherited workspace\n"
    );
    assert!(!repo.join("inherited-restored.txt").exists());
    let status = host
        .agents
        .status(child_id, &json!({"sessionId":grandchild.id}))?;
    assert_eq!(status["workspace"], info.path.to_str().unwrap());
    assert_eq!(status["branch"], info.branch);
    host.stop().await?;
    Ok(())
}
