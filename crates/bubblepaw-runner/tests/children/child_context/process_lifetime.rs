use super::*;

#[tokio::test]
async fn isolated_children_share_exec_admission_and_background_job_lifetime() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repo = directory.path().join("repo");
    initialize(&repo)?;
    let host = Host::start(
        directory.path(),
        &repo,
        &config("[agents]\nauto_wake=false\n[exec]\nsync_timeout_secs=0\nmax_jobs=1\n"),
    )
    .await?;
    let owner = host.store.create_session("parent")?.id;

    // Host cleanup cancels a child's jobs when its run ends (agents.rs cancel_owner), so
    // the job, the descendant probe and the cancel all happen inside child A's live run.
    // Child A starts the job, spawns descendant B, and waits for B before cancelling its own job.
    let marker = "cwd-a.txt";
    let probe_body = format!(
        "{} const refusal=await outcome(()=>tools.exec({{command:{}}})); const foreign=await outcome(()=>tools.poll({{handle:handle}})); return {{refusal,foreign}};",
        OUTCOME,
        json!(quick_command())
    );
    let a = host.spawn(
        &owner,
        &format!(
            "const job=await tools.exec({{command:{long}}}); \
const probeCode='const handle='+JSON.stringify(job.handle)+';'+{probe}; \
const spawned=await tools.agent({{task:JSON.stringify({{code:probeCode}}),profile:'workspace',worktree:true}}); \
await wait({{handle:spawned.handle}}); \
const polled=await tools.poll({{handle:job.handle}}); \
const cancelled=await tools.cancel({{handle:job.handle}}); \
const admitted=await tools.exec({{command:{quick}}}); \
return {{status:job.status,handle:job.handle,polled:polled.status,cancelled:cancelled.status,admittedHandle:admitted.handle}};",
            long = json!(long_command(marker)),
            probe = json!(probe_body),
            quick = json!(quick_command())
        ),
    )?;
    host.finish(&owner, &a).await?;
    let a_path = worktrees::get(&host.store, &a)?.unwrap().path;
    let b = host
        .store
        .list_sessions()?
        .into_iter()
        .find(|session| session.parent_session_id.as_deref() == Some(a.as_str()))
        .unwrap()
        .id;
    let info = worktrees::get(&host.store, &b)?.unwrap();
    assert_eq!(info.parent_workspace, a_path);

    let started = host.results(&a)?[0].clone();
    assert_eq!(started["status"], "backgrounded", "{started}");
    assert!(
        matches!(started["polled"].as_str(), Some("running" | "backgrounded")),
        "{started}"
    );
    assert_eq!(started["cancelled"], "cancelled", "{started}");
    // Cancelling frees the shared slot: A's next exec was admitted.
    assert!(started["admittedHandle"].is_string(), "{started}");
    let written = a_path.join(marker);
    eventually(&format!("{} to be written", written.display()), || {
        written.exists()
    })
    .await;
    assert!(!repo.join(marker).exists());

    // Descendant B ran inside A's live run: admission refused it, and the foreign job stayed closed.
    let probe = host.results(&b)?[0].clone();
    assert_eq!(probe["refusal"]["ok"], false, "exec was admitted: {probe}");
    assert!(
        probe["refusal"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("max_jobs")),
        "{probe}"
    );
    assert_eq!(probe["foreign"]["ok"], false, "poll was allowed: {probe}");
    assert!(
        probe["foreign"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("another conversation")),
        "{probe}"
    );
    host.stop().await?;
    Ok(())
}
