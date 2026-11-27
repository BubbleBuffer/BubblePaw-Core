use crate::{exec_shell::Shell, plain_path::plain_path};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tokio::process::Command;

/// Wraps a POSIX script so the shell records its final directory on exit, but
/// only when it differs from `start`; unchanged runs leave no spool file. The
/// trap writes only the marker file, so stdout, stderr and the exit status are
/// unchanged. The trap sits on line 1, so script line numbers in diagnostics stay.
/// `pwd` prints the directory: `pwd -P`, or `pwd -W` for Git Bash (a Windows path).
fn posix_cwd_marker(script: &str, start: &str, marker: &str, pwd: &str) -> String {
    fn quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
    let report = format!(
        "if [ \"$({pwd})\" != {} ]; then {pwd} >{}; fi 2>/dev/null",
        quote(start),
        quote(marker)
    );
    format!("trap {} EXIT; {script}", quote(&report))
}
fn with_cwd_marker(script: &str, start: &Path, marker: &Path, shell: &Shell) -> String {
    match shell {
        #[cfg(windows)]
        Shell::Cmd => cmd_cwd_marker(script, marker),
        #[cfg(not(windows))]
        Shell::Cmd => script.to_owned(),
        Shell::Posix { git_bash: true, .. } => posix_cwd_marker(
            script,
            &plain_path(start).replace('\\', "/"),
            &plain_path(marker).replace('\\', "/"),
            "pwd -W",
        ),
        Shell::Posix { .. } => posix_cwd_marker(
            script,
            &start.to_string_lossy(),
            &marker.to_string_lossy(),
            "pwd -P",
        ),
    }
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
fn cmd_cwd_marker(script: &str, marker: &Path) -> String {
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

pub(super) fn shell_command(
    script: &str,
    start: &Path,
    marker: Option<&Path>,
    shell: &Shell,
) -> Command {
    let script = match marker {
        Some(marker) => with_cwd_marker(script, start, marker, shell),
        None => script.to_owned(),
    };
    match shell {
        Shell::Cmd => cmd_command(&script),
        Shell::Posix { program, git_bash } => {
            let mut command = Command::new(program);
            // MSYS globs an unquoted argument that holds `*` or `?`, so the
            // script is always quoted, in the form Windows programs parse.
            #[cfg(windows)]
            if *git_bash {
                command
                    .as_std_mut()
                    .raw_arg(format!("-c {}", quote_argument(&script)));
                return command;
            }
            let _ = git_bash;
            command.args(["-c", &script]);
            command
        }
    }
}
#[cfg(windows)]
fn cmd_command(script: &str) -> Command {
    let mut command = Command::new("cmd.exe");
    // /s strips exactly the outer quote pair. Raw source must bypass Rust's
    // MSVCRT argument quoting, including a quoted executable at its beginning.
    command
        .as_std_mut()
        .raw_arg(format!("/d /s /c \"{script}\""));
    command
}
#[cfg(not(windows))]
fn cmd_command(_script: &str) -> Command {
    unreachable!("cmd.exe is only selected on Windows")
}
/// Always-quoted form of one argument (CommandLineToArgvW rules).
#[cfg(windows)]
fn quote_argument(text: &str) -> String {
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for c in text.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                quoted.push(c);
            }
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes + 1));
                quoted.push(c);
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                quoted.push(c);
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes));
    quoted.push('"');
    quoted
}

/// Git Bash takes a Windows path as its working directory; the verbatim form
/// that canonicalization yields is not reliably understood by the MSYS runtime.
pub(super) fn start_directory(cwd: &Path, shell: Option<&Shell>) -> PathBuf {
    match shell {
        Some(Shell::Posix { git_bash: true, .. }) => PathBuf::from(plain_path(cwd)),
        _ => cwd.to_owned(),
    }
}

/// Apply after the optional inherited environment snapshot replaces the host's.
pub(super) fn configure_environment(command: &mut Command, marker: Option<&Path>, shell: &Shell) {
    #[cfg(windows)]
    match shell {
        Shell::Cmd => {
            if let Some(marker) = marker {
                command.env(marker_environment_name(marker), windows_path_text(marker));
            }
        }
        Shell::Posix {
            program,
            git_bash: true,
        } => {
            // As Git Bash's own profile does: GNU tools before Windows look-alikes.
            let directories = crate::exec_shell::tool_directories(program);
            if !directories.is_empty() {
                let current = command
                    .as_std()
                    .get_envs()
                    .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
                    .and_then(|(_, value)| value.map(ToOwned::to_owned))
                    .or_else(|| std::env::var_os("PATH"))
                    .unwrap_or_default();
                if let Ok(path) = std::env::join_paths(
                    directories
                        .into_iter()
                        .chain(std::env::split_paths(&current)),
                ) {
                    command.env("PATH", path);
                }
            }
        }
        Shell::Posix { .. } => {}
    }
    #[cfg(not(windows))]
    let _ = (command, marker, shell);
}

/// The reset note when the marker reports a final directory other than the
/// start directory. A missing, empty or partial marker yields no note.
pub(super) fn cwd_note(marker: &Path, start: &Path, shell: &Shell) -> Option<String> {
    match shell {
        Shell::Cmd => cmd_note(marker, start),
        Shell::Posix { git_bash: true, .. } => git_bash_note(marker, start),
        Shell::Posix { .. } => posix_note(marker, start),
    }
}
#[cfg(unix)]
fn posix_note(marker: &Path, start: &Path) -> Option<String> {
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
#[cfg(not(unix))]
fn posix_note(_marker: &Path, _start: &Path) -> Option<String> {
    None
}
#[cfg(windows)]
fn cmd_note(marker: &Path, start: &Path) -> Option<String> {
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
#[cfg(not(windows))]
fn cmd_note(_marker: &Path, _start: &Path) -> Option<String> {
    None
}
/// Git Bash's `pwd -W` writes a UTF-8 Windows path with forward slashes and a
/// newline; a missing newline is a partial write.
#[cfg(windows)]
fn git_bash_note(marker: &Path, start: &Path) -> Option<String> {
    let length = fs::metadata(marker).ok()?.len();
    if length == 0 || length > 4096 {
        return None;
    }
    let text = String::from_utf8(fs::read(marker).ok()?).ok()?;
    let reported = text.strip_suffix('\n')?.trim_end_matches('\r');
    let backslashed = |text: &str| plain_path(Path::new(text)).replace('/', "\\");
    if reported.is_empty()
        || backslashed(reported).eq_ignore_ascii_case(&backslashed(&start.to_string_lossy()))
    {
        return None;
    }
    Some(cwd_reset_note(start))
}
#[cfg(not(windows))]
fn git_bash_note(_marker: &Path, _start: &Path) -> Option<String> {
    None
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
    plain_path(path)
}
fn cwd_reset_note(start: &Path) -> String {
    format!(
        "Working directory reset to {}; each exec starts there. Pass `cwd` to run elsewhere.",
        plain_path(start)
    )
}

#[cfg(all(test, windows))]
#[path = "process_shell_windows.rs"]
mod windows_tests;
