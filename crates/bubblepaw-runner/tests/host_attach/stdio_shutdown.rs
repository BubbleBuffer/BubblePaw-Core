use super::*;

#[tokio::test]
async fn stdio_shutdown_keeps_an_attached_run_alive() {
    use tokio::io::AsyncWriteExt;
    let lab = Lab::new();
    let data = lab.data();
    let workspace = lab.dir("workspace");
    let mut command = lab.command(&data, &workspace, "fake/code-model", &[], &["stdio"]);
    command.stdin(Stdio::piped());
    let mut host = Proc::spawn(lab.root(), "stdio-host", command);
    host.until("host.json", || host_json(&data).is_some()).await;
    let mut client = Proc::spawn(
        lab.root(),
        "client",
        lab.command(
            &data,
            &workspace,
            "fake/code-model",
            &[],
            &["run", "sleep 4000"],
        ),
    );
    client
        .until("attached model step", || lab.steps_for("sleep 4000") > 0)
        .await;
    host.child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"shutdown\",\"params\":{}}\n")
        .await
        .unwrap();
    let out = host.out.clone();
    host.until("stdio shutdown response", || {
        std::fs::read_to_string(&out)
            .unwrap_or_default()
            .contains("\"stopped\":true")
    })
    .await;
    assert!(client.running());
    assert!(
        host.running(),
        "host must wait after its stdio request stops"
    );
    assert!(client.finish().await.success(), "{}", client.stderr());
    assert_eq!(
        result_record(&client.stdout())["run"]["status"],
        "completed"
    );
    assert!(host.finish().await.success(), "{}", host.stderr());
    assert!(!data.join("host.json").exists());
}
