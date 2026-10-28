use super::*;

#[cfg(windows)]
#[path = "../support/windows_command.rs"]
mod windows_command;

#[tokio::test]
async fn successful_exec_optional_diagnostic_can_be_returned_directly() {
    let f = Fixture::with_native_tools();
    #[cfg(windows)]
    let command = "<nul set /p=optional-field-ok& cmd /c exit 0";
    #[cfg(not(windows))]
    let command = "printf optional-field-ok";
    let result = f.cell("main", &format!("const r=await tools.exec({{command:{}}}); if(r.diagnostic!==undefined) throw Error('unexpected diagnostic'); state.completed=r.status; return {{status:r.status,diagnostic:r.diagnostic,stdout:r.stdout}};", json!(command))).await;
    assert_eq!(
        result,
        json!({"status":"exited","stdout":"optional-field-ok"})
    );
    assert_eq!(f.cell("main", "state.completed").await, "exited");
    assert_eq!(
        f.manager.status(&f.session)["environments"][0]["generation"],
        1
    );
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn failed_cell_retains_successful_command_incomplete_capture_without_raw_stdout() {
    let f = Fixture::with_native_tools();
    #[cfg(windows)]
    let script = windows_command::powershell_file(
        f._root.path(),
        "incomplete-capture.ps1",
        "[Console]::Out.Write(('x' * 5461488))",
    );
    #[cfg(not(windows))]
    let script = "head -c 5461488 /dev/zero | tr '\\000' x";
    let error = f.cell_error(&format!("const p=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:p.handle}}); const out=await tools.poll({{handle:p.handle}}); if(out.capture.stdout.gaps.length) throw Error('Could not reliably enumerate full tree');", serde_json::to_string(&script).unwrap())).await;
    assert!(
        error.contains("Could not reliably enumerate full tree"),
        "{error}"
    );
    assert!(error.contains("Command succeeded (exit code 0)"), "{error}");
    assert!(error.contains("5461488 total bytes"), "{error}");
    assert!(error.contains("omitted bytes"), "{error}");
    assert!(error.contains("smaller output or batches"), "{error}");
    assert!(error.contains("file({path})"), "{error}");
    assert!(!error.contains(&"x".repeat(100)));
    assert!(error.len() < 2048);
    let inspect = f
        .manager
        .control(&f.session, json!({"action":"inspect","environment":"main"}))
        .unwrap();
    assert_eq!(inspect["last_error"], error);
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn failed_command_has_exit_stderr_and_can_be_corrected_without_stale_evidence() {
    let f = Fixture::with_native_tools();
    #[cfg(windows)]
    let bad = "<nul set /p=wrong option: --bogus>&2& exit 7";
    #[cfg(not(windows))]
    let bad = "printf 'wrong option: --bogus' >&2; exit 7";
    #[cfg(windows)]
    let good = "<nul set /p=corrected& cmd /c exit 0";
    #[cfg(not(windows))]
    let good = "printf corrected";
    let error=f.cell_error(&format!("const p=await tools.exec({{command:{},yieldMs:0}}); const done=await wait({{handle:p.handle}}); if(done.exitCode!==0) throw Error('command attempt failed');",serde_json::to_string(bad).unwrap())).await;
    assert!(error.contains("code 7"), "{error}");
    assert!(error.contains("wrong option: --bogus"), "{error}");
    f.cell("main", &format!("const p=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:p.handle}}); text((await tools.poll({{handle:p.handle}})).stdout);",serde_json::to_string(good).unwrap())).await;
    assert_eq!(f.texts(), ["corrected"]);
    // Also clear earlier failure evidence within the same cell after a corrected command.
    let error=f.cell_error(&format!("const bad=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:bad.handle}}); const good=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:good.handle}}); throw Error('unrelated JS error');",serde_json::to_string(bad).unwrap(),serde_json::to_string(good).unwrap())).await;
    assert!(
        error.starts_with("unrelated JS error (line 1, column "),
        "{error}"
    );
    assert!(!error.contains("Last native command outcome"), "{error}");
    let schema = f.cell_error("await tools.exec({command:123});").await;
    assert!(schema.contains("exec"), "{schema}");
    assert!(schema.contains("/command"), "{schema}");
    assert!(
        schema.contains("string") || schema.contains("object"),
        "{schema}"
    );
    let launch = f
        .cell_error("await tools.exec({command:cmd`bubblepaw_missing_executable_92`});")
        .await;
    assert!(launch.contains("could not launch"), "{launch}");
    assert!(launch.contains("exec"), "{launch}");
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}

#[tokio::test]
async fn cmd_tag_exec_roundtrips_native_argv_and_raw_strings_remain_shell() {
    let f = Fixture::with_native_tools();
    let source = f._root.path().join("probe.rs");
    std::fs::write(
        &source,
        "fn main(){ println!(\"{:?}\", std::env::args().skip(1).collect::<Vec<_>>()); }",
    )
    .unwrap();
    let name = if cfg!(windows) {
        "argv_probe.exe"
    } else {
        "argv_probe"
    };
    let compiled = std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(f._root.path().join(name))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let args = [
        "space value",
        "O'Brien",
        "embedded\"quote",
        "$(effect)",
        "`effect`",
        "",
        "line\nnext",
        "雪🐾",
        "; echo effect > escaped.txt",
    ];
    let code = format!(
        r#"
      const args = {};
      const p = await tools.exec({{command:cmd`./{} ${{args[0]}} ${{args[1]}} ${{args[2]}} ${{args[3]}} ${{args[4]}} ${{args[5]}} ${{args[6]}} ${{args[7]}} ${{args[8]}}`,yieldMs:0}});
      await wait({{handle:p.handle,timeoutMs:5000}});
      const out = await tools.poll({{handle:p.handle}});
      text(out.stdout);
    "#,
        serde_json::to_string(&args).unwrap(),
        name
    );
    f.cell("main", &code).await;
    assert_eq!(f.texts(), [format!("{args:?}\n")]);
    assert!(!f._root.path().join("escaped.txt").exists());
    #[cfg(windows)]
    let raw = "echo first&& <nul set /p=second& cmd /c exit 0";
    #[cfg(not(windows))]
    let raw = "printf first\nprintf '\\n'\nprintf second";
    f.cell("main", &format!("const p=await tools.exec({{command:{},yieldMs:0}}); await wait({{handle:p.handle,timeoutMs:5000}}); text((await tools.poll({{handle:p.handle}})).stdout);",serde_json::to_string(raw).unwrap())).await;
    assert_eq!(f.texts()[1].replace("\r\n", "\n"), "first\nsecond");
    f.manager.shutdown().await;
    f._processes.as_ref().unwrap().shutdown().await;
}
