//! `read_first`: workspace files an assignment names, read before the first model call and
//! sent in the first user message. Any bad entry rejects the whole assignment.
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{
    fs,
    io::ErrorKind,
    path::{Component, Path},
};

/// Bytes of `read_first` content allowed per assignment, all files together.
pub const MAX_BYTES: usize = 64 * 1024;

/// One `read_first` entry: a workspace-relative path, optionally with a 1-based inclusive
/// line range written `path:start-end`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadFirst {
    /// The entry as written, trimmed; messages and block headings name it.
    pub entry: String,
    pub path: String,
    pub lines: Option<(usize, usize)>,
}

impl ReadFirst {
    /// Syntax only. Files are checked by [`section`], which knows the workspace.
    pub fn parse(entry: &str) -> Result<Self> {
        let entry = entry.trim();
        ensure!(!entry.is_empty(), "read_first entries must not be empty");
        ensure!(
            !is_absolute(entry),
            "read_first `{entry}` is absolute; paths are relative to the workspace"
        );
        let (path, lines) = match entry.rsplit_once(':') {
            Some((path, range)) => {
                let lines = parse_range(range).ok_or_else(|| {
                    anyhow!(
                        "read_first `{entry}` has a malformed line range; write `path:start-end` with 1-based lines and start <= end"
                    )
                })?;
                (path, Some(lines))
            }
            None => (entry, None),
        };
        ensure!(!path.is_empty(), "read_first `{entry}` names no path");
        Ok(Self {
            entry: entry.to_owned(),
            path: path.to_owned(),
            lines,
        })
    }
}

fn is_absolute(entry: &str) -> bool {
    entry.starts_with(['/', '\\'])
        || Path::new(entry)
            .components()
            .next()
            .is_some_and(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
}

fn parse_range(range: &str) -> Option<(usize, usize)> {
    let (start, end) = range.split_once('-')?;
    let number = |text: &str| -> Option<usize> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    let (start, end) = (number(start)?, number(end)?);
    (start >= 1 && end >= start).then_some((start, end))
}

/// The `## Read first` section for `entries`, or an empty string when there are none.
/// Every entry is resolved and read here; the first bad one rejects the assignment.
pub fn section(entries: &[ReadFirst], workspace: &Path) -> Result<String> {
    if entries.is_empty() {
        return Ok(String::new());
    }
    let root = workspace
        .canonicalize()
        .with_context(|| format!("workspace {} is not readable", workspace.display()))?;
    let mut total = 0usize;
    let mut out = String::from("## Read first\n");
    for entry in entries {
        let content = read_entry(&root, entry)?;
        total += content.len();
        ensure!(
            total <= MAX_BYTES,
            "read_first `{}` takes the contents over the {MAX_BYTES}-byte total; trim the ranges or drop entries",
            entry.entry
        );
        out.push('\n');
        out.push_str(&block(&entry.entry, &content));
    }
    Ok(out.trim_end().to_owned())
}

fn read_entry(root: &Path, entry: &ReadFirst) -> Result<String> {
    let found = match root.join(&entry.path).canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            bail!(
                "read_first `{}` is not a file in the workspace",
                entry.entry
            )
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read_first `{}`", entry.entry));
        }
    };
    ensure!(
        found.starts_with(root),
        "read_first `{}` resolves outside the workspace",
        entry.entry
    );
    ensure!(
        found.is_file(),
        "read_first `{}` is not a regular file",
        entry.entry
    );
    let bytes = fs::read(&found).with_context(|| format!("read_first `{}`", entry.entry))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| anyhow!("read_first `{}` is not UTF-8 text", entry.entry))?;
    let Some((start, end)) = entry.lines else {
        return Ok(text);
    };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    ensure!(
        end <= lines.len(),
        "read_first `{}` ends at line {end}, but the file has {} lines",
        entry.entry,
        lines.len()
    );
    Ok(lines[start - 1..end].concat())
}

/// One heading and fenced block. The fence is longer than any backtick run in the content,
/// so the content cannot close it.
fn block(entry: &str, content: &str) -> String {
    let longest = content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat((longest + 1).max(3));
    let newline = if content.is_empty() || content.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("### `{entry}`\n\n{fence}\n{content}{newline}{fence}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(entries: &[&str]) -> Vec<ReadFirst> {
        entries
            .iter()
            .map(|e| ReadFirst::parse(e).unwrap())
            .collect()
    }

    #[test]
    fn parses_paths_with_and_without_ranges() {
        let whole = ReadFirst::parse(" docs/x.md ").unwrap();
        assert_eq!(whole.entry, "docs/x.md");
        assert_eq!(whole.path, "docs/x.md");
        assert_eq!(whole.lines, None);
        let ranged = ReadFirst::parse("crates/a/src/lib.rs:10-80").unwrap();
        assert_eq!(ranged.path, "crates/a/src/lib.rs");
        assert_eq!(ranged.lines, Some((10, 80)));
        assert_eq!(ReadFirst::parse("a.md:3-3").unwrap().lines, Some((3, 3)));
    }

    #[test]
    fn rejects_bad_entries_by_syntax() {
        let cases = [
            "",
            "   ",
            "/etc/passwd",
            "\\windows\\x.md",
            "C:/x.md",
            ":1-2",
            "a.md:0-3",
            "a.md:5-3",
            "a.md:1",
            "a.md:x-2",
            "a.md:1-2-3",
            "a.md:-2",
            "a.md:1-",
        ];
        for case in cases {
            let error = ReadFirst::parse(case).unwrap_err().to_string();
            assert!(!error.is_empty(), "{case} should be rejected");
        }
        let error = ReadFirst::parse("a.md:5-3").unwrap_err().to_string();
        assert!(error.contains("a.md:5-3"), "{error}");
    }

    #[test]
    fn empty_list_gives_no_section() {
        assert_eq!(section(&[], Path::new(".")).unwrap(), "");
    }

    #[test]
    fn reads_whole_files_and_ranges_into_blocks() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        fs::create_dir_all(ws.join("docs")).unwrap();
        fs::write(ws.join("docs/a.md"), "one\ntwo\nthree\n").unwrap();
        let text = section(&parse_all(&["docs/a.md", "docs/a.md:2-3"]), &ws).unwrap();
        assert_eq!(
            text,
            "## Read first\n\n### `docs/a.md`\n\n```\none\ntwo\nthree\n```\n\n### `docs/a.md:2-3`\n\n```\ntwo\nthree\n```"
        );
    }

    #[test]
    fn fence_outlasts_backticks_in_the_content() {
        let text = block("n.md", "a ```` b\n");
        assert!(text.contains("`````\na ```` b\n`````"), "{text}");
        assert!(block("n.md", "plain\n").contains("```\nplain\n```"));
        assert!(block("n.md", "no newline").contains("no newline\n```\n"));
    }

    #[test]
    fn rejects_a_missing_file_by_name() {
        let root = tempfile::tempdir().unwrap();
        let error = section(&parse_all(&["nope.md"]), root.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("`nope.md`"), "{error}");
        assert!(error.contains("not a file in the workspace"), "{error}");
    }

    #[test]
    fn rejects_paths_that_leave_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        fs::create_dir(&ws).unwrap();
        fs::write(root.path().join("secret.md"), "outside\n").unwrap();
        let error = section(&parse_all(&["../secret.md"]), &ws)
            .unwrap_err()
            .to_string();
        assert!(error.contains("`../secret.md`"), "{error}");
        assert!(error.contains("resolves outside the workspace"), "{error}");
        // A `..` that stays inside is fine.
        fs::create_dir(ws.join("sub")).unwrap();
        fs::write(ws.join("in.md"), "inside\n").unwrap();
        assert!(section(&parse_all(&["sub/../in.md"]), &ws).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_that_resolves_outside() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        fs::create_dir(&ws).unwrap();
        fs::write(root.path().join("secret.md"), "outside\n").unwrap();
        std::os::unix::fs::symlink(root.path().join("secret.md"), ws.join("link.md")).unwrap();
        let error = section(&parse_all(&["link.md"]), &ws)
            .unwrap_err()
            .to_string();
        assert!(error.contains("`link.md`"), "{error}");
        assert!(error.contains("resolves outside the workspace"), "{error}");
    }

    #[test]
    fn rejects_out_of_bounds_ranges() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.md"), "one\ntwo\n").unwrap();
        fs::write(root.path().join("empty.md"), "").unwrap();
        let error = section(&parse_all(&["a.md:2-9"]), root.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("`a.md:2-9`"), "{error}");
        assert!(error.contains("ends at line 9"), "{error}");
        assert!(section(&parse_all(&["a.md:3-3"]), root.path()).is_err());
        assert!(section(&parse_all(&["empty.md:1-1"]), root.path()).is_err());
        assert!(section(&parse_all(&["a.md:2-2"]), root.path()).is_ok());
    }

    #[test]
    fn rejects_directories_and_non_utf8_files() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("bin.dat"), [0xff_u8, 0xfe, 0x00]).unwrap();
        let dir = section(&parse_all(&["dir"]), root.path())
            .unwrap_err()
            .to_string();
        assert!(
            dir.contains("`dir`") && dir.contains("not a regular file"),
            "{dir}"
        );
        let bin = section(&parse_all(&["bin.dat"]), root.path())
            .unwrap_err()
            .to_string();
        assert!(
            bin.contains("`bin.dat`") && bin.contains("not UTF-8"),
            "{bin}"
        );
    }

    #[test]
    fn rejects_a_total_over_the_cap_naming_the_entry_that_crosses_it() {
        let root = tempfile::tempdir().unwrap();
        let half = "x".repeat(MAX_BYTES / 2 + 1);
        fs::write(root.path().join("one.txt"), &half).unwrap();
        fs::write(root.path().join("two.txt"), &half).unwrap();
        // One file under the cap is fine on its own.
        assert!(section(&parse_all(&["one.txt"]), root.path()).is_ok());
        let error = section(&parse_all(&["one.txt", "two.txt"]), root.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("`two.txt`"), "{error}");
        assert!(error.contains("65536-byte total"), "{error}");
    }
}
