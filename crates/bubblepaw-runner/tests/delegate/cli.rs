use super::*;

#[path = "../support/fixture_binary.rs"]
mod fixture_binary;

#[tokio::test]
async fn cli_delegate_writes_the_ledger_and_reports() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace)?;
    let file = root.path().join("task.md");
    std::fs::write(
        &file,
        "+++\ncategory = \"research\"\nchecks = [\"exit 0\"]\nroute = { model = \"demo/scripted\" }\n+++\nList the files.\n",
    )?;
    let data = root.path().join("data");
    let run = |args: &[&str]| {
        let mut command = std::process::Command::new(fixture_binary::resolve(
            env!("CARGO_BIN_EXE_bubblepaw"),
            "bubblepaw",
        ));
        command
            .args([
                "--provider",
                "demo",
                "--read-only",
                "--no-auto-summarize",
                "--workspace",
            ])
            .arg(&workspace)
            .arg("--data-dir")
            .arg(&data)
            .args(args)
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("OPENAI_API_KEY");
        command.output()
    };
    let output = run(&["--json", "delegate", file.to_str().unwrap(), "--seed", "1"])?;
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap_or("{}"))
        .map_err(|e| anyhow::anyhow!("{e}: {stdout}\n{stderr}"))?;
    assert_eq!(last["type"], "delegation", "{stdout}\n{stderr}");
    assert_eq!(last["model"], "demo/scripted");
    assert_eq!(last["route_reason"], "named");
    // The demo's prose reply is not a valid final report, so the run fails.
    assert_eq!(last["status"], "failed", "{stdout}\n{stderr}");
    assert!(!output.status.success());
    assert!(stdout.lines().count() > 1, "events precede the record");
    let report = run(&["delegations", "report"])?;
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report: Value = serde_json::from_slice(&report.stdout)?;
    assert_eq!(report["groups"][0]["category"], "research");
    assert_eq!(report["groups"][0]["class"], "named");
    assert_eq!(report["groups"][0]["accepted"], 0);
    // Unknown categories are rejected before anything runs.
    std::fs::write(
        &file,
        "+++\ncategory = \"dance\"\nroute = { class = \"small\" }\n+++\nx\n",
    )?;
    let rejected = run(&["delegate", file.to_str().unwrap()])?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unknown assignment category"));
    Ok(())
}
