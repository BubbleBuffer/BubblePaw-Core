use super::*;

#[tokio::test]
async fn live_exec_output_cap_applies_only_to_new_jobs() {
    let fixture = Fixture::new(ExecConfig {
        max_output_bytes: 1000,
        ..ExecConfig::default()
    });
    #[cfg(windows)]
    let delayed_output = windows_command::powershell_file(
        fixture._workspace.path(),
        "cap-reload.ps1",
        "Start-Sleep -Milliseconds 300; [Console]::Out.Write(('a' * 10000))",
    );
    #[cfg(not(windows))]
    let delayed_output = r"sleep 0.3; head -c 10000 /dev/zero | tr '\000' a";
    let old = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":delayed_output,"background":true}),
    )
    .await;
    fixture.processes.apply_config(ExecConfig {
        max_output_bytes: 2500,
        ..ExecConfig::default()
    });
    let new = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":delayed_output,"background":true}),
    )
    .await;
    for receipt in [&old, &new] {
        assert_eq!(
            call(
                &fixture.registry,
                "owner",
                "wait",
                json!({"handle":receipt["handle"],"timeoutMs":60000})
            )
            .await["status"],
            "exited"
        );
    }
    let jobs = fixture.processes.jobs("owner").unwrap();
    let old_job = jobs["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["handle"] == old["handle"])
        .unwrap();
    let new_job = jobs["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["handle"] == new["handle"])
        .unwrap();
    assert_eq!(old_job["spooledBytes"], 1000);
    assert_eq!(new_job["spooledBytes"], 2500);
    assert_eq!(spool_size(&fixture.data.path().join("jobs")), 3500);
    fixture.processes.shutdown().await;
}
