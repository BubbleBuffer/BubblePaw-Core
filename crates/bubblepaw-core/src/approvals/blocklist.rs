//! Hardline floor for `exec` commands (docs/approvals.md). The gate checks it
//! before configured denies, session allowances, run overrides and every mode,
//! so no allowance reaches these commands. It is best-effort insurance, not a
//! sandbox: it folds case, quotes, carets and braces, reads command words per
//! shell segment, skips common wrappers and follows `cd` within one command.
//! It does not model every shell.

use crate::ApprovalPolicy;
use serde_json::Value;
use std::path::PathBuf;

/// Prefixes that run the command after them: privilege and environment
/// wrappers, and shells that take the command as an argument.
const WRAPPERS: &[&str] = &[
    "sudo",
    "doas",
    "env",
    "nohup",
    "time",
    "command",
    "exec",
    "nice",
    "timeout",
    "xargs",
    "start",
    "cmd",
    "cmd.exe",
    "powershell",
    "powershell.exe",
    "pwsh",
    "pwsh.exe",
    "sh",
    "bash",
    "zsh",
    "dash",
];
/// cmd switches before a wrapped command, such as `cmd /c format c:`.
const SWITCHES: &[&str] = &["/c", "/k", "/s", "/q", "/d"];
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

impl ApprovalPolicy {
    /// The hardline pattern this call matches. None when the floor is off or
    /// the call is not an `exec` command on the list.
    ///
    /// `workspaces` lists the roots this call's session may touch. The first is
    /// the directory the exec tool runs in: its `cwd` argument resolves against
    /// it, and `cd` inside the command moves from there. Every listed root is
    /// protected. Without roots, only the literal workspace forms (`.`) match.
    pub fn hardline_match(
        &self,
        tool: &str,
        arguments: &Value,
        workspaces: &[PathBuf],
    ) -> Option<String> {
        if !self.hardline || tool != "exec" {
            return None;
        }
        let command = command_text(arguments.get("command")?)?;
        let roots: Vec<Place> = workspaces
            .iter()
            .map(|path| Place::new(&path.to_string_lossy()))
            .collect();
        let start = roots.first().and_then(|root| {
            let cwd = arguments.get("cwd").and_then(Value::as_str).unwrap_or(".");
            resolve(&Place::new(cwd), Some(root))
        });
        segment_readings(&command).iter().find_map(|segments| {
            let mut cwd = start.clone();
            segments.iter().find_map(|words| {
                let found = segment_match(words, &roots, cwd.as_ref());
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

/// Words per segment, read two ways: quote-aware, so a quoted path with spaces
/// stays one word, and plain, where quotes only vanish. Both readings must pass,
/// so an unbalanced quote cannot hide a later command.
fn segment_readings(command: &str) -> [Vec<Vec<String>>; 2] {
    let folded: String = command
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '`' | '^' | '{' | '}'))
        .collect();
    [
        split_segments(&folded, true),
        split_segments(&folded, false),
    ]
}

/// Newlines and shell operators end a segment; whitespace ends a word.
fn split_segments(folded: &str, quote_aware: bool) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    for c in folded.chars() {
        if let Some(open) = quote {
            if c == open {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '"' | '\'' if quote_aware => quote = Some(c),
            '"' | '\'' => {}
            '\n' | '\r' | ';' | '&' | '|' | '(' | ')' => {
                end_word(&mut words, &mut word);
                if !words.is_empty() {
                    segments.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => end_word(&mut words, &mut word),
            c => word.push(c),
        }
    }
    end_word(&mut words, &mut word);
    if !words.is_empty() {
        segments.push(words);
    }
    segments
}

fn end_word(words: &mut Vec<String>, word: &mut String) {
    if !word.is_empty() {
        words.push(std::mem::take(word));
    }
}

/// The command name and its arguments, after the wrappers, switches, options,
/// assignments and numeric arguments that stand before the command word.
fn command_word(words: &[String]) -> Option<(&str, &[String])> {
    let mut words = words;
    while let Some(first) = words.first() {
        let first = first.as_str();
        let skip = WRAPPERS.contains(&first)
            || SWITCHES.contains(&first)
            || first.starts_with('-')
            || first.contains('=')
            || first.chars().all(|c| c.is_ascii_digit() || c == '.');
        if !skip {
            break;
        }
        words = &words[1..];
    }
    let (head, args) = words.split_first()?;
    let name = head.rsplit(['/', '\\']).next()?.trim_end_matches(".exe");
    Some((name, args))
}

fn segment_match(words: &[String], roots: &[Place], cwd: Option<&Place>) -> Option<String> {
    let (name, args) = command_word(words)?;
    match name {
        "git" => git_match(args),
        "rm" => delete_match("rm -rf", args, roots, cwd),
        "remove-item" | "ri" => delete_match("Remove-Item -Recurse -Force", args, roots, cwd),
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
        .find_map(|target| target_class(target, roots, cwd))?;
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
fn target_class(target: &str, roots: &[Place], cwd: Option<&Place>) -> Option<&'static str> {
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
    if matches!(
        base.trim_end_matches(['/', '\\']),
        "~" | "$home" | "%userprofile%" | "$env:userprofile"
    ) {
        return Some("the home directory");
    }
    let place = Place::new(base);
    match resolve(&place, cwd) {
        Some(path) if path.is_root() => Some("the filesystem root"),
        Some(path) if path.is_drive_root() => Some("a drive root"),
        Some(path) => (!contents && roots.contains(&path)).then_some("the workspace root"),
        None => (!contents && place.parts.is_empty()).then_some("the workspace root"),
    }
}

/// `target` as a place: absolute targets stand alone, others extend `base`.
/// None when the relative target has no known base.
fn resolve(target: &Place, base: Option<&Place>) -> Option<Place> {
    if target.absolute {
        return Some(target.clone());
    }
    base.map(|base| base.join(target))
}

/// A path read lexically: its drive, whether it is rooted and its normal parts.
/// Nothing touches the filesystem, so a missing path still has a place.
#[derive(Clone, Debug, PartialEq)]
struct Place {
    drive: String,
    absolute: bool,
    parts: Vec<String>,
}

impl Place {
    /// Lowercased, with `/` separators, a Windows verbatim prefix (`\\?\`) removed,
    /// and `.` and `..` folded.
    fn new(path: &str) -> Self {
        let folded = path.to_lowercase().replace('\\', "/");
        let folded = folded.strip_prefix("//?/").unwrap_or(&folded);
        let (drive, rest) = match folded.as_bytes() {
            [letter, b':', ..] if letter.is_ascii_alphabetic() => folded.split_at(2),
            _ => ("", folded),
        };
        let absolute = !drive.is_empty() || rest.starts_with('/');
        let mut parts = Vec::new();
        push_parts(&mut parts, absolute, rest.split('/'));
        Self {
            drive: drive.to_owned(),
            absolute,
            parts,
        }
    }

    fn join(&self, relative: &Place) -> Place {
        let mut parts = self.parts.clone();
        push_parts(
            &mut parts,
            self.absolute,
            relative.parts.iter().map(String::as_str),
        );
        Place {
            drive: self.drive.clone(),
            absolute: self.absolute,
            parts,
        }
    }

    fn is_root(&self) -> bool {
        self.absolute && self.drive.is_empty() && self.parts.is_empty()
    }

    fn is_drive_root(&self) -> bool {
        self.absolute && !self.drive.is_empty() && self.parts.is_empty()
    }
}

/// Appends path segments, folding `.` and `..`. A rooted path cannot climb above its root.
fn push_parts<'a>(
    parts: &mut Vec<String>,
    absolute: bool,
    segments: impl Iterator<Item = &'a str>,
) {
    for segment in segments {
        match segment {
            "" | "." => {}
            ".." => {
                let climb = parts.last().is_some_and(|last| last.as_str() != "..");
                if climb {
                    parts.pop();
                } else if !absolute {
                    parts.push(segment.to_owned());
                }
            }
            other => parts.push(other.to_owned()),
        }
    }
}

/// A drive letter with an optional separator, such as `C:` or `d:\`.
fn is_drive(text: &str) -> bool {
    let text = text.trim_end_matches(['/', '\\']);
    let bytes = text.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// A short-option cluster that carries `flag`, such as `-f` or `-fdx`. Never a long option.
fn has_short_flag(arg: &str, flag: char) -> bool {
    arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") && arg.contains(flag)
}

#[cfg(test)]
#[path = "blocklist_tests.rs"]
mod tests;
