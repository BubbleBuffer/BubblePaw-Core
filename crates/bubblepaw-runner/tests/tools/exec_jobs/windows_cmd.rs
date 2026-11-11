use super::*;

#[tokio::test]
async fn default_shell_is_cmd_not_powershell() {
    let fixture = Fixture::new(ExecConfig::default());
    // cmd.exe has no Write-Output cmdlet: the command fails instead of printing.
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":"Write-Output bp-default-shell"}),
    )
    .await;
    assert_eq!(result["status"], "exited");
    assert_ne!(result["exitCode"], 0);
    assert_eq!(result["stdout"], "");
    assert!(result.get("note").is_none());
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn cmd_joins_quoted_ampersands_and_percent_variables_after_cd() {
    let fixture = Fixture::new(ExecConfig::default());
    let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
    std::fs::create_dir(fixture._workspace.path().join("sub")).unwrap();
    let comspec = std::env::var("ComSpec").unwrap();
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":r#"cd sub && echo "a && b" && echo %ComSpec%"#}),
    )
    .await;
    assert_eq!(result["status"], "exited", "{result}");
    assert_eq!(result["exitCode"], 0);
    assert_eq!(
        result["stdout"].as_str().unwrap().replace("\r\n", "\n"),
        format!("\"a && b\" \n{comspec}\n")
    );
    assert_eq!(result["stderr"], "");
    assert_eq!(result["note"], cwd_reset_note(&start));
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn cmd_cd_then_exact_nonzero_exit_and_new_cwd_are_both_reported() {
    let fixture = Fixture::new(ExecConfig::default());
    let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
    std::fs::create_dir(fixture._workspace.path().join("sub")).unwrap();
    for code in [5, 9] {
        let result = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":format!("cd sub && <nul set /p=out& 1>&2 <nul set /p=err& cmd /c exit {code}")}),
        )
        .await;
        assert_eq!(result["status"], "exited", "{result}");
        assert_eq!(result["exitCode"], code);
        assert_eq!(result["stdout"], "out");
        assert_eq!(result["stderr"].as_str().unwrap().trim_end(), "err");
        assert_eq!(result["note"], cwd_reset_note(&start));
    }
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn cmd_keeps_native_exit_code_and_marks_no_note_when_directory_is_unchanged() {
    let fixture = Fixture::new(ExecConfig::default());
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":"<nul set /p=out& cmd /c exit 7"}),
    )
    .await;
    assert_eq!(result["exitCode"], 7, "{result}");
    assert_eq!(result["stdout"], "out");
    assert!(result.get("note").is_none());
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn cmd_keeps_literal_exclamation_marks_without_delayed_expansion() {
    let fixture = Fixture::new(ExecConfig::default());
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":"echo wow!!&& echo !ComSpec!&& echo a!b!c"}),
    )
    .await;
    assert_eq!(result["status"], "exited", "{result}");
    assert_eq!(result["exitCode"], 0);
    assert_eq!(
        result["stdout"].as_str().unwrap().replace("\r\n", "\n"),
        "wow!!\n!ComSpec!\na!b!c\n"
    );
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn cmd_explicit_exit_keeps_its_code_and_writes_no_note() {
    let fixture = Fixture::new(ExecConfig::default());
    let elsewhere = tempfile::tempdir().unwrap();
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":format!("cd \"{}\" && exit /b 4", elsewhere.path().display())}),
    )
    .await;
    assert_eq!(result["exitCode"], 4, "{result}");
    assert!(result.get("note").is_none());
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn background_directory_note_appears_on_final_wait_and_poll() {
    let fixture = Fixture::new(ExecConfig::default());
    let elsewhere = tempfile::tempdir().unwrap();
    let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
    let started = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({
            "command":format!("cd /d \"{}\" && <nul set /p=now& ping -n 2 127.0.0.1 >nul& <nul set /p=late& 1>&2 <nul set /p=err& cmd /c exit 3", elsewhere.path().display()),
            "background":true
        }),
    )
    .await;
    assert_eq!(started["status"], "running");
    assert!(started.get("note").is_none());
    let waited = call(
        &fixture.registry,
        "owner",
        "wait",
        json!({"handle":started["handle"],"timeoutMs":60000}),
    )
    .await;
    assert_eq!(waited["status"], "exited");
    assert_eq!(waited["exitCode"], 3);
    assert_eq!(waited["note"], cwd_reset_note(&start));
    let polled = call(
        &fixture.registry,
        "owner",
        "poll",
        json!({"handle":started["handle"]}),
    )
    .await;
    assert_eq!(polled["exitCode"], 3);
    assert_eq!(polled["stdout"], "nowlate");
    assert_eq!(polled["stderr"], "err");
    assert_eq!(polled["note"], cwd_reset_note(&start));
    fixture.processes.shutdown().await;
}
