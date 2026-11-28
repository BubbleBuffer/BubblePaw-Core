//! Hardline floor for `exec` commands (docs/approvals.md). The gate checks it
//! before configured denies, session allowances, run overrides and every mode,
//! so no allowance reaches these commands. It is best-effort insurance, not a
//! sandbox: it folds case, quotes, backslash escapes, carets and braces, reads
//! command words per shell segment (`blocklist_words.rs`), skips common wrappers
//! and follows `cd` within one command. Places (`blocklist_place.rs`) are read
//! lexically, with the host's home directory in every spelling. It does not
//! model every shell.

use crate::ApprovalPolicy;
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::path::PathBuf;

#[path = "blocklist_place.rs"]
mod place;
#[path = "blocklist_words.rs"]
mod words;

use place::{Place, host_home, is_drive};
use words::{command_word, segment_readings};

/// Commands that change the directory for the segments after them.
const CD_COMMANDS: &[&str] = &[
    "cd",
    "chdir",
    "pushd",
    "set-location",
    "sl",
    "push-location",
];
/// Options that take no directory, so the directory is the next word.
const CD_OPTIONS: &[&str] = &["/d", "-path", "-literalpath"];

/// Selects the first string among `cwd` and `workdir`, with `.` as the default.
/// The second tuple element validates both arguments and returns `workdir`
/// when both were supplied, so exec can compare the resolved paths.
/// The hardline matcher uses the selected string even if validation fails.
pub fn exec_working_directory(arguments: &Value) -> (&str, Result<Option<&str>>) {
    let cwd_value = arguments.get("cwd");
    let workdir_value = arguments.get("workdir");
    let cwd = cwd_value.and_then(Value::as_str);
    let workdir = workdir_value.and_then(Value::as_str);
    let alias = match (cwd_value, workdir_value) {
        (Some(_), _) if cwd.is_none() => Err(anyhow!("cwd must be a string")),
        (_, Some(_)) if workdir.is_none() => Err(anyhow!("workdir must be a string")),
        _ => Ok(cwd.and(workdir)),
    };
    (cwd.or(workdir).unwrap_or("."), alias)
}

impl ApprovalPolicy {
    /// The hardline pattern this call matches. None when the floor is off or
    /// the call is not an `exec` command on the list.
    ///
    /// `workspaces` lists the protected roots this session may touch. The first
    /// is the exec tool's default directory and the base for relative `cwd` and
    /// `workdir` arguments. A `cd` can move the directory for later segments.
    /// Without roots, only the literal workspace forms (`.`) match.
    pub fn hardline_match(
        &self,
        tool: &str,
        arguments: &Value,
        workspaces: &[PathBuf],
    ) -> Option<String> {
        self.hardline_match_at(tool, arguments, workspaces, host_home())
    }

    /// `hardline_match` with the home directory given, so tests need not
    /// depend on the real one.
    fn hardline_match_at(
        &self,
        tool: &str,
        arguments: &Value,
        workspaces: &[PathBuf],
        home: &Place,
    ) -> Option<String> {
        if !self.hardline || tool != "exec" {
            return None;
        }
        let command = command_text(arguments.get("command")?)?;
        let (directory, _) = exec_working_directory(arguments);
        let roots: Vec<Place> = workspaces
            .iter()
            .map(|path| Place::new(&path.to_string_lossy()))
            .collect();
        let start = roots
            .first()
            .and_then(|root| resolve(&Place::new(directory), Some(root)));
        segment_readings(&command).iter().find_map(|segments| {
            let mut cwd = start.clone();
            segments.iter().find_map(|words| {
                let found = segment_match(words, &roots, home, cwd.as_ref());
                if !roots.is_empty()
                    && let Some(next) = cd_target(words, cwd.as_ref())
                {
                    cwd = next;
                }
                found
            })
        })
    }

    /// Hardline is a host setting: a run override or child profile cannot turn it off.
    pub fn with_inherited_hardline(mut self, parent: &Self) -> Self {
        self.hardline = parent.hardline;
        self
    }
}

/// The message a model sees for a hardline match. A person cannot approve it.
pub(super) fn hardline_message(pattern: &str) -> String {
    format!(
        "Blocked by hardline rule {pattern}. This command never runs, even when approvals allow it; choose a narrower command."
    )
}

/// Text a shell would run. A cmd template runs without a shell, so its parts
/// and values are joined with spaces to read as one command line.
fn command_text(command: &Value) -> Option<String> {
    if let Some(text) = command.as_str() {
        return Some(text.to_owned());
    }
    let parts = command.get("parts")?.as_array()?;
    let values = command.get("values")?.as_array()?;
    let mut text = String::new();
    for (index, part) in parts.iter().enumerate() {
        text.push_str(part.as_str()?);
        text.push(' ');
        if let Some(value) = values.get(index) {
            text.push_str(value.as_str()?);
            text.push(' ');
        }
    }
    Some(text)
}

fn segment_match(
    words: &[String],
    roots: &[Place],
    home: &Place,
    cwd: Option<&Place>,
) -> Option<String> {
    let (name, args) = command_word(words)?;
    match name {
        "git" => git_match(args),
        "rm" => delete_match("rm -rf", args, roots, home, cwd),
        "remove-item" | "ri" => delete_match("Remove-Item -Recurse -Force", args, roots, home, cwd),
        "shutdown" => Some("shutdown".into()),
        "reboot" => Some("reboot".into()),
        "format" | "format.com" => args
            .iter()
            .any(|arg| is_drive(arg))
            .then(|| "format of a drive letter".into()),
        "dd" => args
            .iter()
            .any(|arg| arg.starts_with("of=/dev/"))
            .then(|| "dd to /dev/*".into()),
        _ if name.starts_with("mkfs") => Some("mkfs".into()),
        _ => None,
    }
}

/// The directory a `cd`-style segment leaves the shell in. `Some(None)` when that
/// directory is no longer known; None when the segment does not change it.
fn cd_target(words: &[String], cwd: Option<&Place>) -> Option<Option<Place>> {
    let (name, args) = command_word(words)?;
    if !CD_COMMANDS.contains(&name) {
        return None;
    }
    let mut targets = args
        .iter()
        .filter(|arg| !CD_OPTIONS.contains(&arg.as_str()));
    let (Some(target), None) = (targets.next(), targets.next()) else {
        return Some(None);
    };
    // Home, variables and `cd -` depend on the shell's state, so they are unknown.
    if target.starts_with(['~', '$', '%']) || target == "-" {
        return Some(None);
    }
    Some(resolve(&Place::new(target), cwd))
}

fn git_match(args: &[String]) -> Option<String> {
    // Global options precede the subcommand; `-c`, `-C`, `--git-dir`,
    // `--work-tree` and `--namespace` take a separate value.
    let mut index = 0;
    while index < args.len() && args[index].starts_with('-') {
        index += match args[index].as_str() {
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" => 2,
            _ => 1,
        };
    }
    let subcommand = args.get(index)?;
    let rest = args.get(index + 1..).unwrap_or(&[]);
    match subcommand.as_str() {
        "reset" if rest.iter().any(|arg| arg.as_str() == "--hard") => {
            Some("git reset --hard".into())
        }
        // `--force-with-lease` and `--force-if-includes` are different options.
        "push"
            if rest
                .iter()
                .any(|arg| arg.as_str() == "--force" || has_short_flag(arg, 'f')) =>
        {
            Some("git push --force".into())
        }
        "clean"
            if rest
                .iter()
                .any(|arg| arg.as_str() == "--force" || has_short_flag(arg, 'f')) =>
        {
            Some("git clean -f".into())
        }
        _ => None,
    }
}

/// A recursive and force delete of a protected place. `label` names the command.
fn delete_match(
    label: &str,
    args: &[String],
    roots: &[Place],
    home: &Place,
    cwd: Option<&Place>,
) -> Option<String> {
    let (mut recursive, mut force) = (false, false);
    let mut targets = Vec::new();
    for arg in args {
        if let Some(long) = arg.strip_prefix("--") {
            recursive |= long == "recursive";
            force |= long == "force";
        } else if let Some(switches) = arg.strip_prefix('-').filter(|s| !s.is_empty()) {
            let (r, f) = switch_flags(switches);
            recursive |= r;
            force |= f;
        } else {
            targets.push(arg.as_str());
        }
    }
    if !(recursive && force) {
        return None;
    }
    let class = targets
        .into_iter()
        .find_map(|target| target_class(target, roots, home, cwd))?;
    Some(format!("{label} of {class}"))
}

/// Recursive and force switches in one dash argument: PowerShell names with
/// any prefix (`-r`, `-Recurse`, `-fo`, `-Force`), otherwise a short cluster (`-rf`).
fn switch_flags(switches: &str) -> (bool, bool) {
    if "recurse".starts_with(switches) {
        (true, false)
    } else if switches.len() >= 2 && "force".starts_with(switches) {
        (false, true)
    } else {
        (switches.contains('r'), switches.contains('f'))
    }
}

/// The protected place a delete target names, resolved against the directory
/// the command is in. Contents of a root or home count; other paths do not,
/// nor do `*` and `./*`. Without a known directory, a relative target matches
/// only when it names the current directory itself (`.`), which is the
/// conservative reading.
fn target_class(
    target: &str,
    roots: &[Place],
    home: &Place,
    cwd: Option<&Place>,
) -> Option<&'static str> {
    let (base, contents) = match target
        .strip_suffix("/*")
        .or_else(|| target.strip_suffix("\\*"))
    {
        Some(base) => (base, true),
        None => (target, false),
    };
    if contents && base.is_empty() {
        return Some("the filesystem root");
    }
    let place = Place::under_home(base, home).unwrap_or_else(|| Place::new(base));
    match resolve(&place, cwd) {
        Some(path) if path.is_root() => Some("the filesystem root"),
        Some(path) if path.is_drive_root() => Some("a drive root"),
        Some(path) if path.is_home(home) => Some("the home directory"),
        Some(path) if path.is_above(home) => Some("a parent of the home directory"),
        Some(path) => (!contents && roots.contains(&path)).then_some("the workspace root"),
        None => (!contents && place.is_empty()).then_some("the workspace root"),
    }
}

/// `target` as a place: absolute targets stand alone, others extend `base`.
/// None when the relative target has no known base.
fn resolve(target: &Place, base: Option<&Place>) -> Option<Place> {
    if target.is_absolute() {
        return Some(target.clone());
    }
    base.map(|base| base.join(target))
}

/// A short-option cluster that carries `flag`, such as `-f` or `-fdx`. Never a long option.
fn has_short_flag(arg: &str, flag: char) -> bool {
    arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") && arg.contains(flag)
}

#[cfg(test)]
#[path = "blocklist_tests.rs"]
mod tests;
