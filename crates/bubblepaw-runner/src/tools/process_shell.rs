#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{fs, path::Path};
use tokio::process::Command;

/// Wraps a POSIX script so the shell records its final directory on exit, but
/// only when it differs from `start`; unchanged runs leave no spool file. The
/// trap writes only the marker file, so stdout, stderr and the exit status are
/// unchanged. The trap sits on line 1, so script line numbers in diagnostics stay.
#[cfg(unix)]
fn with_cwd_marker(script: &str, start: &Path, marker: &Path) -> String {
    fn quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
    let report = format!(
        "if [ \"$(pwd -P)\" != {} ]; then pwd -P >{}; fi 2>/dev/null",
        quote(&start.to_string_lossy()),
        quote(&marker.to_string_lossy())
    );
    format!("trap {} EXIT; {script}", quote(&report))
}
/// CALL is used only for numeric status, never for a directory or marker path:
/// its second parse would interpret shell metacharacters in those paths. The
/// nested Unicode cmd inherits the final directory and reports it with `cd`.
/// Clear a shadowing ERRORLEVEL environment variable only after user code. SET
/// preserves the internal status. Numeric capture stays unquoted: quoting would
/// keep the caret literal and prevent CALL's runtime expansion.
/// Explicit `exit` (including `exit /b`) bypasses the suffix, as cmd normally
/// does; no marker is available in that case, but its exit code is preserved.
#[cfg(windows)]
fn with_cwd_marker(script: &str, _start: &Path, marker: &Path) -> String {
    let name = marker_environment_name(marker);
    format!(
        r#"{script}&set "ERRORLEVEL="&call set {name}_RC=%^ERRORLEVEL%&cmd.exe /d /u /c cd >"%{name}%" 2>nul&call exit /b %^{name}_RC%"#,
    )
}
/// The invocation UUID gives private bookkeeping variables distinct names.
/// The path is supplied through the environment, so `%` in a directory name
/// is never parsed as source. No delayed expansion is enabled (`!` stays data).
#[cfg(windows)]
fn marker_environment_name(marker: &Path) -> String {
    format!(
        "BUBBLEPAW_CWD_{}",
        marker
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .replace('-', "_")
    )
}

pub(super) fn shell_command(script: &str, start: &Path, marker: Option<&Path>) -> Command {
    let script = match marker {
        Some(marker) => with_cwd_marker(script, start, marker),
        None => script.to_owned(),
    };
    #[cfg(windows)]
    let mut command = Command::new("cmd.exe");
    #[cfg(windows)]
    // /s strips exactly the outer quote pair. Raw source must bypass Rust's
    // MSVCRT argument quoting, including a quoted executable at its beginning.
    command
        .as_std_mut()
        .raw_arg(format!("/d /s /c \"{script}\""));
    #[cfg(not(windows))]
    let mut command = Command::new("sh");
    #[cfg(not(windows))]
    command.args(["-c", &script]);
    command
}

/// Apply after the optional inherited environment snapshot replaces the host's.
pub(super) fn configure_marker_environment(command: &mut Command, marker: Option<&Path>) {
    #[cfg(windows)]
    if let Some(marker) = marker {
        command.env(marker_environment_name(marker), windows_path_text(marker));
    }
    #[cfg(not(windows))]
    let _ = (command, marker);
}
#[cfg(not(any(unix, windows)))]
fn with_cwd_marker(script: &str, _start: &Path, _marker: &Path) -> String {
    script.to_owned()
}
/// The reset note when the marker reports a final directory other than the
/// start directory. A missing, empty or partial marker yields no note.
#[cfg(unix)]
pub(super) fn cwd_note(marker: &Path, start: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let length = fs::metadata(marker).ok()?.len();
    if length == 0 || length > 4096 {
        return None;
    }
    let bytes = fs::read(marker).ok()?;
    // `pwd -P` ends with a newline; anything else is a partial write.
    let reported = bytes.strip_suffix(b"\n")?;
    if reported.is_empty() {
        return None;
    }
    // Both sides are physical paths: `pwd -P` and the canonical start directory.
    if Path::new(std::ffi::OsStr::from_bytes(reported)) == start {
        return None;
    }
    Some(cwd_reset_note(start))
}
#[cfg(windows)]
pub(super) fn cwd_note(marker: &Path, start: &Path) -> Option<String> {
    let length = fs::metadata(marker).ok()?.len();
    if length == 0 || length > 4096 {
        return None;
    }
    let bytes = fs::read(marker).ok()?;
    let reported = read_windows_marker(&bytes)?;
    if reported.is_empty() || reported.eq_ignore_ascii_case(&windows_path_text(start)) {
        return None;
    }
    Some(cwd_reset_note(start))
}
/// `cmd /u /c cd` writes UTF-16LE without a BOM and ends with CRLF. Require
/// that terminator so interrupted/partial reports cannot claim a cwd change.
#[cfg(windows)]
fn read_windows_marker(bytes: &[u8]) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<_> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(units.strip_suffix(&[13, 10])?).ok()
}
/// The marker's `cd` reports ordinary drive/UNC paths, while
/// workspace canonicalization uses extended-length Windows path prefixes.
#[cfg(windows)]
fn windows_path_text(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    }
}
#[cfg(not(any(unix, windows)))]
pub(super) fn cwd_note(_marker: &Path, _start: &Path) -> Option<String> {
    None
}
#[cfg(any(unix, windows))]
fn cwd_reset_note(start: &Path) -> String {
    format!(
        "Working directory reset to {}; each exec starts there. Pass `cwd` to run elsewhere.",
        start.display()
    )
}

#[cfg(all(test, windows))]
#[path = "process_shell_windows.rs"]
mod windows_tests;
