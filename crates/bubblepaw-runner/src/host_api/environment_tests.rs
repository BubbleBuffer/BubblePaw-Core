use super::*;
use anyhow::Result;

#[tokio::test]
async fn real_environment_inspection_and_null_default_control_preserve_status() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("environment inspection", root.path())?;
    let artifacts = Arc::new(crate::artifacts::ArtifactStore::new(
        root.path(),
        root.path().join("artifacts"),
    )?);
    let repl = Arc::new(crate::codemode::CodeMode::new(
        root.path().into(),
        artifacts,
    )?);
    let mut ctx = HostCtx::for_store(store, root.path());
    ctx.repl = Some(repl.clone());
    let created = dispatch(
        &ctx,
        Profile::Full,
        "environment.control",
        json!({"session_id":session.id,"control":{"action":"create","environment":"fixture"}}),
    )
    .await;
    let inspected = dispatch(
        &ctx,
        Profile::Read,
        "environment.inspect",
        json!({"session_id":session.id}),
    )
    .await;
    let default_control = dispatch(
        &ctx,
        Profile::Full,
        "environment.control",
        json!({"session_id":session.id}),
    )
    .await;
    let null_control = dispatch(
        &ctx,
        Profile::Full,
        "environment.control",
        json!({"session_id":session.id,"control":null}),
    )
    .await;
    let direct_default = repl.control(&session.id, json!({"action":"inspect"}));
    let direct_null = repl.control(&session.id, Value::Null);
    let status = repl.status(&session.id);
    repl.shutdown().await;

    assert_eq!(created?["name"], "fixture");
    // Compare the entire nonempty object, including every environment field.
    assert_eq!(inspected?, status);
    assert_eq!(status["environments"].as_array().unwrap().len(), 1);
    for key in [
        "name",
        "generation",
        "execution_id",
        "status",
        "waiting",
        "time_waiting",
        "handles",
        "state",
        "output_count",
        "outputs_omitted",
        "outputs",
        "last_error",
        "result_available",
        "result_execution_id",
        "state_changes",
    ] {
        assert!(
            status["environments"][0].get(key).is_some(),
            "missing environment key: {key}"
        );
    }
    assert_eq!(default_control?, direct_default?);
    assert_eq!(null_control?, direct_null?);
    Ok(())
}
