//! Reading an `exec` command into words per shell segment for the hardline
//! floor (see `blocklist.rs`). Every command is read four ways, and a command
//! is refused when any reading is refused.

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
    "powershell",
    "pwsh",
    "sh",
    "bash",
    "zsh",
    "dash",
];
/// cmd switches before a wrapped command, such as `cmd /c format c:`.
const SWITCHES: &[&str] = &["/c", "/k", "/s", "/q", "/d"];

/// Words per segment. Quote-aware, so a quoted path with spaces stays one
/// word, or plain, where quotes only vanish. And with a backslash as a path
/// separator (cmd), or as an escape (sh and bash). All must pass, so an
/// unbalanced quote or an escaped letter cannot hide a command.
pub(super) fn segment_readings(command: &str) -> [Vec<Vec<String>>; 4] {
    let lowered = command
        .to_lowercase()
        .replace("$(pwd)", ".")
        .replace("`pwd`", ".");
    let folded: String = lowered
        .chars()
        .filter(|c| !matches!(c, '`' | '^' | '{' | '}'))
        // The working directory by variable is `.` (braces are already gone).
        .collect::<String>()
        .replace("$pwd.path", ".")
        .replace("$pwd", ".")
        .replace("%cd%", ".");
    [
        split_segments(&folded, true, false),
        split_segments(&folded, false, false),
        split_segments(&folded, true, true),
        split_segments(&folded, false, true),
    ]
}

/// Newlines and shell operators end a segment; whitespace ends a word. With
/// `escape`, a backslash makes the next character literal, as in sh.
fn split_segments(folded: &str, quote_aware: bool, escape: bool) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut chars = folded.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            if c == open {
                quote = None;
            } else if escape
                && open == '"'
                && c == '\\'
                && let Some(next) = chars.next_if(|next| matches!(next, '"' | '\\' | '$'))
            {
                word.push(next);
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '\\' if escape => match chars.next() {
                Some('\n') | None => {}
                Some(next) => word.push(next),
            },
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
pub(super) fn command_word(words: &[String]) -> Option<(&str, &[String])> {
    let mut words = words;
    while let Some((head, args)) = words.split_first() {
        let first = head.as_str();
        let name = first.rsplit(['/', '\\']).next()?.trim_end_matches(".exe");
        let skip = WRAPPERS.contains(&name)
            || SWITCHES.contains(&first)
            || first.starts_with('-')
            || first.contains('=')
            || first.chars().all(|c| c.is_ascii_digit() || c == '.');
        if !skip {
            return Some((name, args));
        }
        words = args;
    }
    None
}
