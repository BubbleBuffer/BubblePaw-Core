//! The `search` tool: one call answers "what is in this repo" (recursive file
//! listing) and "where is X" (content search). Both honour `.gitignore`.
use super::*;
use ignore::{
    WalkBuilder,
    gitignore::{Gitignore, GitignoreBuilder},
};

pub(super) const FIELDS: &[&str] = &[
    "path",
    "pattern",
    "query",
    "glob",
    "regex",
    "context",
    "filesOnly",
    "maxResults",
];

const FILES_DEFAULT: usize = 500;
const FILES_MAX: usize = 5000;
const MATCHES_DEFAULT: usize = 100;
const MATCHES_MAX: usize = 1000;
const CONTEXT_MAX: usize = 10;
/// Directory entries visited before the walk stops, so a huge tree stays bounded.
const WALK_LIMIT: usize = 100_000;
const LINE_CLIP: usize = 500;
const MAX_SEARCHED_FILE: u64 = 2 * 1024 * 1024;
/// Room kept for the closing note inside the result limit.
const NOTE_ROOM: usize = 200;
const SKIPPED_DIRECTORIES: [&str; 3] = [".git", "node_modules", "target"];

/// Include and exclude glob sets. Patterns follow `.gitignore` syntax; a
/// leading `!` excludes. A file passes when it matches an include (or there is
/// none) and no exclude.
struct Globs {
    include: Option<Gitignore>,
    exclude: Option<Gitignore>,
}
impl Globs {
    fn new(input: &Value, root: &Path) -> Result<Self> {
        let patterns: Vec<String> = match input.get("glob") {
            None => Vec::new(),
            Some(Value::String(pattern)) => vec![pattern.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow!("glob entries must be strings"))
                })
                .collect::<Result<_>>()?,
            Some(_) => bail!("glob must be a string or an array of strings"),
        };
        let (mut include, mut exclude) = (GitignoreBuilder::new(root), GitignoreBuilder::new(root));
        let (mut has_include, mut has_exclude) = (false, false);
        for pattern in &patterns {
            ensure!(!pattern.trim().is_empty(), "glob entries must not be empty");
            let result = match pattern.strip_prefix('!') {
                Some(rest) => {
                    has_exclude = true;
                    exclude.add_line(None, rest)
                }
                None => {
                    has_include = true;
                    include.add_line(None, pattern)
                }
            };
            result.with_context(|| format!("invalid glob {pattern:?}"))?;
        }
        Ok(Self {
            include: has_include.then(|| include.build()).transpose()?,
            exclude: has_exclude.then(|| exclude.build()).transpose()?,
        })
    }
    fn allows(&self, path: &Path) -> bool {
        let hit = |set: &Gitignore| set.matched_path_or_any_parents(path, false).is_ignore();
        self.include.as_ref().is_none_or(hit) && !self.exclude.as_ref().is_some_and(hit)
    }
}

fn flag(input: &Value, key: &str) -> Result<bool> {
    match input.get(key) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| anyhow!("{key} must be a boolean")),
    }
}

fn display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

impl WorkspaceTools {
    /// Files under `path` (recursive, `.gitignore` honoured), or with `pattern`
    /// the lines that match it. The result is capped; `more` counts what the
    /// cap left out.
    pub(super) fn search(
        &self,
        input: &Value,
        cancellation: &bubblepaw_core::CancellationToken,
        relative: &str,
    ) -> Result<Value> {
        let start = self.existing(relative)?;
        // `query` is the older name of `pattern`. Empty or missing means files mode.
        let query = match optional_string(input, "pattern")? {
            Some(pattern) => Some(pattern),
            None => optional_string(input, "query")?,
        }
        .filter(|pattern| !pattern.is_empty());
        let files_only = flag(input, "filesOnly")?;
        let regex = match query {
            Some(query) => {
                let pattern = if flag(input, "regex")? {
                    query.to_owned()
                } else {
                    regex::escape(query)
                };
                Some(regex::Regex::new(&pattern).context("invalid search pattern")?)
            }
            None => {
                flag(input, "regex")?;
                None
            }
        };
        let listing = regex.is_none() || files_only;
        let (default, maximum) = if regex.is_none() {
            (FILES_DEFAULT, FILES_MAX)
        } else {
            (MATCHES_DEFAULT, MATCHES_MAX)
        };
        let limit = count(input, "maxResults", default)?.min(maximum);
        ensure!(limit > 0, "maxResults must be positive");
        let context = count(input, "context", 0)?.min(CONTEXT_MAX);
        let globs = Globs::new(input, &start)?;

        let mut walker = WalkBuilder::new(&start);
        walker
            .hidden(false)
            .require_git(false)
            .git_global(false)
            .follow_links(false)
            .sort_by_file_path(|a, b| a.cmp(b))
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !(entry.file_type().is_some_and(|kind| kind.is_dir())
                        && SKIPPED_DIRECTORIES
                            .iter()
                            .any(|skip| entry.file_name() == *skip))
            });
        let mut entries = Vec::new();
        // Everything found, shown or not: `total - shown` is the "N more" note.
        let (mut total, mut shown, mut visited) = (0usize, 0usize, 0usize);
        let mut walk_cut = false;
        for entry in walker.build() {
            ensure!(!cancellation.is_cancelled(), "cancelled");
            visited += 1;
            if visited > WALK_LIMIT {
                walk_cut = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) || !globs.allows(entry.path())
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let name = display(&self.root, entry.path());
            let Some(regex) = regex.as_ref() else {
                total += 1;
                if shown < limit {
                    shown += 1;
                    entries.push(json!({"path":name,"bytes":metadata.len()}));
                }
                continue;
            };
            if metadata.len() > MAX_SEARCHED_FILE {
                continue;
            }
            let Ok(bytes) = read_bounded(entry.path()) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            let hits: Vec<usize> = (0..lines.len())
                .filter(|&index| regex.is_match(lines[index]))
                .collect();
            if hits.is_empty() {
                continue;
            }
            if files_only {
                total += 1;
                if shown < limit {
                    shown += 1;
                    entries.push(json!({"path":name,"bytes":metadata.len()}));
                }
                continue;
            }
            // Matches that fit the cap, with their context lines merged in line order.
            let mut wanted: BTreeMap<usize, bool> = BTreeMap::new();
            for &hit in &hits {
                total += 1;
                if shown >= limit {
                    continue;
                }
                shown += 1;
                let last = (hit + context).min(lines.len() - 1);
                for index in hit.saturating_sub(context)..=last {
                    wanted.entry(index).or_insert(false);
                }
                wanted.insert(hit, true);
            }
            for (index, is_match) in wanted {
                let text = clip(lines[index], LINE_CLIP).0;
                entries.push(if is_match {
                    json!({"path":name,"line":index + 1,"text":text})
                } else {
                    json!({"path":name,"line":index + 1,"text":text,"context":true})
                });
            }
        }
        let more = total - shown;
        let mut result =
            json!({"more":more,"truncated":more > 0 || walk_cut,"walkLimitReached":walk_cut});
        result[if listing { "files" } else { "matches" }] = Value::Array(entries);
        Ok(result)
    }
}

/// Plain text for a direct model call of `search`: file paths with sizes, or
/// `path:line: text` lines (`path-line- text` for context), then one bracketed
/// note when results were left out.
pub(super) fn render(value: &Value) -> String {
    let files = value.get("files").and_then(Value::as_array);
    let entries = files
        .or_else(|| value["matches"].as_array())
        .map_or(&[][..], Vec::as_slice);
    let budget = MAX_RESULT - NOTE_ROOM;
    let mut out = String::new();
    let mut shown = 0usize;
    let mut previous: Option<(&str, u64)> = None;
    for entry in entries {
        let path = entry["path"].as_str().unwrap_or_default();
        let mut line = String::new();
        if files.is_some() {
            line.push_str(&format!(
                "{path} ({} B)\n",
                entry["bytes"].as_u64().unwrap_or(0)
            ));
        } else {
            let number = entry["line"].as_u64().unwrap_or(0);
            if previous.is_some_and(|(last, at)| last != path || at + 1 != number) {
                line.push_str("--\n");
            }
            let divider = if entry["context"] == true { '-' } else { ':' };
            line.push_str(&format!(
                "{path}{divider}{number}{divider} {}\n",
                entry["text"].as_str().unwrap_or_default()
            ));
            previous = Some((path, number));
        }
        if out.len() + line.len() > budget {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    let left_out = value["more"].as_u64().unwrap_or(0) as usize + (entries.len() - shown);
    let noun = if files.is_some() { "files" } else { "matches" };
    if entries.is_empty() {
        out.push_str(&format!("[no {noun}]"));
    } else if left_out > 0 {
        out.push_str(&format!(
            "[{left_out} more {noun} not shown; narrow with path, glob or pattern, or raise maxResults]"
        ));
    }
    if value["walkLimitReached"] == true {
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push_str("[stopped after 100000 paths; narrow with path or glob]");
    }
    out
}
