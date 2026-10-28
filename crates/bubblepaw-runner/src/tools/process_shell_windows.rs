use super::*;
use std::process::Stdio;

async fn run(
    script: &str,
    start: &Path,
    marker: &Path,
    destination: &Path,
) -> std::process::Output {
    let mut command = shell_command(script, start, Some(marker));
    configure_marker_environment(&mut command, Some(marker));
    command
        .current_dir(start)
        .env("BUBBLEPAW_TEST_DESTINATION", windows_path_text(destination))
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn final_directory_marker_preserves_metacharacters_unicode_and_native_status() {
    let root = tempfile::tempdir().unwrap();
    let start = fs::canonicalize(root.path()).unwrap();
    let destination = start.join("final space & wow! %literal% 雪");
    let reports = start.join("reports space & wow! %literal% 雪");
    fs::create_dir(&destination).unwrap();
    fs::create_dir(&reports).unwrap();
    for code in [0, 1, 7, 37, 255] {
        let marker = reports.join(format!("{}.cwd", uuid::Uuid::new_v4()));
        let script = format!(
            r#"set "BP_RC=unexpected" & cd /d "%BUBBLEPAW_TEST_DESTINATION%" && <nul set /p="out!"&1>&2 <nul set /p="err!"& "%ComSpec%" /d /c exit {code}"#,
        );
        let output = run(&script, &start, &marker, &destination).await;
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        assert_eq!(output.stdout, b"out!");
        assert_eq!(output.stderr, b"err!");
        let bytes = fs::read(&marker).unwrap();
        assert_eq!(
            read_windows_marker(&bytes).as_deref(),
            Some(windows_path_text(&destination).as_str()),
            "marker must contain the actual final directory"
        );
        assert_eq!(cwd_note(&marker, &start), Some(cwd_reset_note(&start)));
    }
}

#[tokio::test]
async fn unchanged_directory_quoted_executable_percent_variable_and_bangs_stay_exact() {
    let root = tempfile::tempdir().unwrap();
    let start = fs::canonicalize(root.path()).unwrap();
    let marker = start.join(format!("{}.cwd", uuid::Uuid::new_v4()));
    let script =
        r#""%ComSpec%" /d /c exit 0 && <nul set /p="a && b %BUBBLEPAW_TEST_VALUE% !ComSpec!""#;
    let mut command = shell_command(script, &start, Some(&marker));
    configure_marker_environment(&mut command, Some(&marker));
    let output = command
        .current_dir(&start)
        .env("BUBBLEPAW_TEST_VALUE", "percent-value")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    // SET /P with closed stdin returns 1, even though it writes its prompt.
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(output.stdout, b"a && b percent-value !ComSpec!");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(
        read_windows_marker(&fs::read(&marker).unwrap()),
        Some(windows_path_text(&start))
    );
    assert_eq!(cwd_note(&marker, &start), None);
}

#[tokio::test]
async fn shadowing_errorlevel_is_visible_to_user_code_but_does_not_replace_exit_status() {
    let root = tempfile::tempdir().unwrap();
    let start = fs::canonicalize(root.path()).unwrap();
    let marker = start.join(format!("{}.cwd", uuid::Uuid::new_v4()));
    let mut command = shell_command(
        r#"<nul set /p="%ERRORLEVEL%"&set "ERRORLEVEL=another-shadow"&cmd.exe /d /c exit 37"#,
        &start,
        Some(&marker),
    );
    configure_marker_environment(&mut command, Some(&marker));
    let output = command
        .current_dir(&start)
        .env("ERRORLEVEL", "user-visible")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(37), "{output:?}");
    assert_eq!(output.stdout, b"user-visible");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(
        read_windows_marker(&fs::read(marker).unwrap()),
        Some(windows_path_text(&start))
    );
}

#[tokio::test]
async fn explicit_exit_bypasses_suffix_and_preserves_status() {
    let root = tempfile::tempdir().unwrap();
    let start = fs::canonicalize(root.path()).unwrap();
    let destination = start.join("different");
    fs::create_dir(&destination).unwrap();
    for exit in ["exit 19", "exit /b 23"] {
        let marker = start.join(format!("{}.cwd", uuid::Uuid::new_v4()));
        let output = run(
            &format!(r#"cd /d "%BUBBLEPAW_TEST_DESTINATION%" && {exit}"#),
            &start,
            &marker,
            &destination,
        )
        .await;
        assert_eq!(
            output.status.code(),
            Some(if exit == "exit 19" { 19 } else { 23 })
        );
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(!marker.exists());
    }
}

#[test]
fn marker_requires_complete_utf16_crlf_report() {
    for malformed in [
        vec![],
        vec![0],
        vec![b'C', 0],
        vec![13, 0],
        vec![0, 216, 13, 0, 10, 0],
    ] {
        assert_eq!(read_windows_marker(&malformed), None);
    }
    let bytes = "C:\\space & ! %literal% 雪\r\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    assert_eq!(
        read_windows_marker(&bytes).as_deref(),
        Some("C:\\space & ! %literal% 雪")
    );
}
