//! Which shell `exec` runs commands in (docs/exec.md). Linux and macOS use
//! `sh -c`. Windows prefers Git for Windows' bash, so the model's bash habits
//! (`tail`, `set -e`, `$(...)`) work, and falls back to `cmd.exe` when Git is
//! not installed. `exec.shell` (or `BUBBLEPAW_FORCE_SHELL` while the setting is
//! `auto`) forces the choice. The WSL launcher `C:\Windows\System32\bash.exe`
//! is never selected: it would start WSL.
use anyhow::{Result, bail, ensure};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// A resolved shell. Cheap to clone; resolve again per exec so settings apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shell {
    /// `cmd.exe /d /s /c` (Windows only).
    Cmd,
    /// A POSIX shell run as `<program> -c <script>`. `git_bash` marks the MSYS
    /// flavour on Windows: Windows paths on its command line, `pwd -W`.
    Posix { program: PathBuf, git_bash: bool },
}

impl Shell {
    /// The name the prompt gives the model.
    pub fn name(&self) -> String {
        match self {
            Self::Cmd => "cmd.exe".into(),
            Self::Posix { git_bash: true, .. } => "bash (Git Bash on Windows)".into(),
            Self::Posix { program, .. } => program
                .file_stem()
                .map_or_else(|| "sh".into(), |stem| stem.to_string_lossy().into_owned()),
        }
    }
    /// The name with its invocation, for the exec tool description.
    pub fn invocation(&self) -> String {
        match self {
            Self::Cmd => "cmd.exe (cmd /d /s /c)".into(),
            Self::Posix { git_bash: true, .. } => "bash (Git Bash on Windows, bash -c; write POSIX syntax and use forward slashes in paths)".into(),
            Self::Posix { .. } => format!("{0} ({0} -c)", self.name()),
        }
    }
}

/// The preference the host loaded, for the prompt, which is built without the
/// exec settings at hand. `exec` itself resolves the live setting every call.
static HOST_PREFERENCE: RwLock<String> = RwLock::new(String::new());

pub fn set_host_preference(preference: &str) {
    *HOST_PREFERENCE.write().expect("shell preference lock") = preference.to_owned();
}

/// The shell the prompt and tool declarations name. A forced shell that cannot
/// be found falls back to the automatic choice here; `exec` reports the error.
pub fn host_shell() -> Shell {
    let preference = HOST_PREFERENCE
        .read()
        .expect("shell preference lock")
        .clone();
    resolve(&preference)
        .unwrap_or_else(|_| choose("auto", None, cfg!(windows), git_bash).expect("automatic shell"))
}

/// The shell description for the exec tool declaration.
pub fn invocation(preference: &str) -> String {
    resolve(preference)
        .unwrap_or_else(|_| host_shell())
        .invocation()
}

/// `auto` (or empty) follows `BUBBLEPAW_FORCE_SHELL`, then picks by platform.
pub fn resolve(preference: &str) -> Result<Shell> {
    let env = std::env::var("BUBBLEPAW_FORCE_SHELL").ok();
    choose(preference, env.as_deref(), cfg!(windows), git_bash)
}

fn choose(
    preference: &str,
    env: Option<&str>,
    windows: bool,
    git_bash: impl FnOnce() -> Option<PathBuf>,
) -> Result<Shell> {
    let mut wanted = preference.trim();
    if wanted.is_empty() || wanted.eq_ignore_ascii_case("auto") {
        wanted = env
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("auto");
    }
    let is = |name: &str| wanted.eq_ignore_ascii_case(name);
    if !windows {
        return match wanted {
            _ if is("auto") || is("sh") => Ok(posix("sh")),
            _ if is("bash") => Ok(posix("bash")),
            _ if is("cmd") || is("cmd.exe") => bail!("exec.shell cmd is only available on Windows"),
            path => Ok(posix(path)),
        };
    }
    if is("cmd") || is("cmd.exe") {
        return Ok(Shell::Cmd);
    }
    if is("auto") {
        return Ok(git_bash().map_or(Shell::Cmd, |program| Shell::Posix {
            program,
            git_bash: true,
        }));
    }
    if is("bash") {
        let Some(program) = git_bash() else {
            bail!(
                "exec.shell is bash but Git for Windows' bash.exe was not found; install Git for Windows or set exec.shell to cmd or to a bash.exe path"
            );
        };
        return Ok(Shell::Posix {
            program,
            git_bash: true,
        });
    }
    let path = PathBuf::from(wanted);
    ensure!(
        !is_wsl_launcher(&path),
        "exec.shell must not be the WSL launcher ({wanted}); use Git for Windows' bash.exe"
    );
    ensure!(path.is_file(), "exec.shell {wanted} is not a file");
    if path
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case("cmd"))
    {
        return Ok(Shell::Cmd);
    }
    Ok(Shell::Posix {
        program: path,
        git_bash: true,
    })
}

fn posix(program: &str) -> Shell {
    Shell::Posix {
        program: program.into(),
        git_bash: false,
    }
}

/// The WSL launcher and the Store alias: running either starts WSL.
pub fn is_wsl_launcher(path: &Path) -> bool {
    let text = path.to_string_lossy().to_lowercase().replace('/', "\\");
    [
        "\\windows\\system32\\",
        "\\windows\\syswow64\\",
        "\\windows\\sysnative\\",
        "\\windowsapps\\",
    ]
    .iter()
    .any(|part| text.contains(part))
}

/// The first existing candidate that is not the WSL launcher.
pub fn select_bash(
    candidates: impl IntoIterator<Item = PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find(|path| !is_wsl_launcher(path) && exists(path))
}

/// Directories to put first on PATH so GNU tools (`tail`, `find`) win over
/// Windows look-alikes, as in Git Bash itself: `<git>\usr\bin` beside `<git>\bin`.
pub fn tool_directories(program: &Path) -> Vec<PathBuf> {
    let Some(directory) = program.parent() else {
        return Vec::new();
    };
    let root = if directory.ends_with("usr/bin") {
        directory.parent().and_then(Path::parent)
    } else {
        directory.parent()
    };
    root.map(|root| root.join("usr").join("bin"))
        .filter(|path| path.is_dir())
        .into_iter()
        .collect()
}

/// Git for Windows' bash: its registry install path, `git.exe` on PATH, then the
/// standard install folders. Found once per process.
fn git_bash() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let roots = git_roots();
            let candidates = roots.into_iter().flat_map(|root| {
                [
                    root.join("bin").join("bash.exe"),
                    root.join("usr").join("bin").join("bash.exe"),
                ]
            });
            select_bash(candidates, |path| path.is_file())
        })
        .clone()
}

#[cfg(windows)]
fn git_roots() -> Vec<PathBuf> {
    use std::os::windows::process::CommandExt;
    let mut roots = Vec::new();
    for hive in ["HKCU", "HKLM"] {
        let output = std::process::Command::new("reg")
            .args([
                "query",
                &format!(r"{hive}\SOFTWARE\GitForWindows"),
                "/v",
                "InstallPath",
            ])
            .creation_flags(0x0800_0000)
            .output();
        if let Ok(output) = output
            && let Some(line) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .find(|line| line.contains("REG_SZ"))
            && let Some((_, path)) = line.split_once("REG_SZ")
        {
            roots.push(PathBuf::from(path.trim()));
        }
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if dir.join("git.exe").is_file() {
            // `<git>\cmd\git.exe` and `<git>\mingw64\bin\git.exe`.
            roots.extend(dir.parent().map(Path::to_path_buf));
            roots.extend(dir.parent().and_then(Path::parent).map(Path::to_path_buf));
        }
    }
    for (variable, suffix) in [
        ("ProgramFiles", "Git"),
        ("ProgramW6432", "Git"),
        ("ProgramFiles(x86)", "Git"),
        ("LOCALAPPDATA", r"Programs\Git"),
    ] {
        if let Some(base) = std::env::var_os(variable) {
            roots.push(PathBuf::from(base).join(suffix));
        }
    }
    roots
}

#[cfg(not(windows))]
fn git_roots() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wsl_launcher_is_never_selected_even_when_it_is_the_only_bash() {
        let system32 = PathBuf::from(r"C:\Windows\System32\bash.exe");
        let store = PathBuf::from(r"C:\Users\dev\AppData\Local\Microsoft\WindowsApps\bash.exe");
        let git = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        let present = |_: &Path| true;
        assert_eq!(
            select_bash([system32.clone(), store.clone()], present),
            None
        );
        assert_eq!(
            select_bash([system32, store, git.clone()], present),
            Some(git)
        );
        assert!(choose(r"C:\Windows\system32\bash.exe", None, true, || None).is_err());
    }

    #[test]
    fn forced_choice_beats_the_environment_and_auto_follows_it() {
        let git = || Some(PathBuf::from(r"C:\Git\bin\bash.exe"));
        assert_eq!(choose("cmd", Some("bash"), true, git).unwrap(), Shell::Cmd);
        assert_eq!(choose("auto", Some("cmd"), true, git).unwrap(), Shell::Cmd);
        assert!(matches!(
            choose("auto", None, true, git).unwrap(),
            Shell::Posix { git_bash: true, .. }
        ));
        assert_eq!(choose("", None, true, || None).unwrap(), Shell::Cmd);
        assert!(choose("bash", None, true, || None).is_err());
        assert_eq!(choose("auto", None, false, git).unwrap().name(), "sh");
    }
}
