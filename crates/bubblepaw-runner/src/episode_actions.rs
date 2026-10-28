//! Parses the `Actions:` log of a v3 episode summary into rows. Pure and lenient:
//! malformed lines are reported and skipped, never fatal.
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ActionVerb {
    Read,
    Searched,
    Edited,
    Created,
    Deleted,
    Moved,
    Ran,
    Asked,
    Answered,
    Delegated,
    Decided,
    Corrected,
}
impl ActionVerb {
    const ALL: [Self; 12] = [
        Self::Read,
        Self::Searched,
        Self::Edited,
        Self::Created,
        Self::Deleted,
        Self::Moved,
        Self::Ran,
        Self::Asked,
        Self::Answered,
        Self::Delegated,
        Self::Decided,
        Self::Corrected,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::Searched => "Searched",
            Self::Edited => "Edited",
            Self::Created => "Created",
            Self::Deleted => "Deleted",
            Self::Moved => "Moved",
            Self::Ran => "Ran",
            Self::Asked => "Asked",
            Self::Answered => "Answered",
            Self::Delegated => "Delegated",
            Self::Decided => "Decided",
            Self::Corrected => "Corrected",
        }
    }
    /// A listed verb, or the listed verb a close synonym stands for.
    fn parse(word: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|verb| verb.as_str() == word)
            .or(match word {
                "Rewrote" | "Modified" | "Updated" => Some(Self::Edited),
                "Wrote" => Some(Self::Created),
                "Executed" => Some(Self::Ran),
                "Listed" => Some(Self::Read),
                _ => None,
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    Decision,
    Error,
    Correction,
    Question,
    Risk,
}
impl ActionKind {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "decision" => Self::Decision,
            "error" => Self::Error,
            "correction" => Self::Correction,
            "question" => Self::Question,
            "risk" => Self::Risk,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ActionRow {
    pub verb: ActionVerb,
    pub target: String,
    /// `+N −M`, `created` or `deleted`, exactly as written.
    pub change: Option<String>,
    /// One or more observed results; several are kept joined by ` · `.
    pub outcome: Option<String>,
    pub call_id: Option<String>,
    pub kind: Option<ActionKind>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ActionLog {
    /// False when the summary has no `Actions:` section at all.
    pub present: bool,
    pub rows: Vec<ActionRow>,
    /// Lines inside the section that did not match the grammar.
    pub malformed: Vec<String>,
}

/// Parses the first `Actions:` section. `Actions: none` yields a present, empty log.
pub fn parse_actions(summary: &str) -> ActionLog {
    let mut log = ActionLog::default();
    let mut lines = summary.lines().map(str::trim);
    for line in lines.by_ref() {
        if let Some(rest) = section_label(line, "actions") {
            log.present = true;
            if rest.eq_ignore_ascii_case("none") {
                return log;
            }
            if !rest.is_empty() {
                log.malformed.push(rest.to_owned());
            }
            break;
        }
    }
    if !log.present {
        return log;
    }
    let mut started = false;
    for line in lines {
        if line.is_empty() {
            if started {
                break;
            }
            continue;
        }
        let Some(item) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .map(str::trim)
        else {
            break;
        };
        started = true;
        if item.eq_ignore_ascii_case("none") {
            continue;
        }
        match parse_row(item) {
            Some(row) => log.rows.push(row),
            None => log.malformed.push(item.to_owned()),
        }
    }
    log
}

/// The gist: the first non-empty line after the `# ` title, without inline-code
/// markers. None when there is no title or the next line is another heading.
pub fn parse_gist(summary: &str) -> Option<String> {
    let mut lines = summary
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    lines.next()?.strip_prefix("# ")?;
    let gist = lines
        .take_while(|line| {
            section_label(line, "actions").is_none() && section_label(line, "open").is_none()
        })
        .find(|line| !is_structure(line))?;
    let words: Vec<&str> = gist.split_whitespace().take(GIST_WORDS).collect();
    Some(words.join(" ").replace('`', ""))
}

/// The most words of a gist that [`parse_gist`] keeps.
pub const GIST_WORDS: usize = 25;

/// Headings, list items, quotes, tables and fences are not gist prose.
fn is_structure(line: &str) -> bool {
    line.starts_with(['#', '>', '|'])
        || line.starts_with("```")
        || line.starts_with("- ")
        || line.starts_with("* ")
        || line.starts_with("+ ")
        || line
            .split_once(". ")
            .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Accepts `Actions:`, `Actions: none`, `## Actions` and `**Actions:**`.
fn section_label<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let line = line.trim_start_matches('#').trim();
    let line = line.strip_prefix("**").unwrap_or(line);
    let head = line.get(..name.len())?;
    if !head.eq_ignore_ascii_case(name) {
        return None;
    }
    let rest = &line[name.len()..];
    let rest = rest.strip_prefix("**").unwrap_or(rest);
    let rest = match rest.strip_prefix(':') {
        Some(rest) => rest,
        None if rest.trim().is_empty() || rest.starts_with("**") => rest,
        None => return None,
    };
    Some(rest.trim_start_matches('*').trim())
}

fn parse_row(item: &str) -> Option<ActionRow> {
    let (body, point) = match item.split_once(" ! ") {
        Some((body, point)) => (body.trim(), Some(point.trim())),
        None => (item, None),
    };
    let (kind, reason) = match point {
        Some(point) => {
            let (kind, reason) = point.split_once(':')?;
            let reason = reason.trim();
            (
                Some(ActionKind::parse(kind.trim())?),
                (!reason.is_empty()).then(|| reason.to_owned()),
            )
        }
        None => (None, None),
    };
    let (body, call_id) = match body.strip_suffix(']').and_then(|b| b.rsplit_once(" [")) {
        Some((body, id)) if !id.is_empty() && !id.contains(char::is_whitespace) => {
            (body.trim(), Some(id.to_owned()))
        }
        _ => (body, None),
    };
    let mut parts = body.split(" · ").map(str::trim);
    let (verb, target) = parts.next()?.split_once(' ')?;
    let verb = ActionVerb::parse(verb)?;
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    let mut change = None;
    let mut outcomes = Vec::new();
    for part in parts {
        if part.is_empty() {
            return None;
        }
        if change.is_none() && outcomes.is_empty() && is_change(part) {
            change = Some(part.to_owned());
        } else {
            outcomes.push(part);
        }
    }
    let outcome = (!outcomes.is_empty()).then(|| outcomes.join(" · "));
    Some(ActionRow {
        verb,
        target: target.to_owned(),
        change,
        outcome,
        call_id,
        kind,
        reason,
    })
}

/// `created`, `deleted` or `+N −M` (ASCII `-` accepted for the minus).
pub fn is_change(part: &str) -> bool {
    if part == "created" || part == "deleted" {
        return true;
    }
    let Some((added, removed)) = part.split_once(' ') else {
        return false;
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    added.strip_prefix('+').is_some_and(digits)
        && removed
            .strip_prefix('−')
            .or_else(|| removed.strip_prefix('-'))
            .is_some_and(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str = "# Fix episode prompt\nAdded v3 and checked it.\n\nProgress: done.\n\nActions:\n- Read 6 files in crates/x\n- Edited crates/a.rs · +12 −3 [call_4]\n- Created docs/b.md · created [call_5]\n- Ran cargo test -p bubblepaw-runner · failed 2 [call_6] ! error: two parser tests failed\n- Decided keep v2 default ! decision: no behaviour change\n- Asked \"which model?\" [msg_9] ! question: route unclear\nOpen: none\n";

    #[test]
    fn parses_valid_rows() {
        let log = parse_actions(SUMMARY);
        assert!(log.present);
        assert!(log.malformed.is_empty(), "{:?}", log.malformed);
        assert_eq!(log.rows.len(), 6);
        assert_eq!(log.rows[0].verb, ActionVerb::Read);
        assert_eq!(log.rows[0].target, "6 files in crates/x");
        assert_eq!(
            log.rows[1],
            ActionRow {
                verb: ActionVerb::Edited,
                target: "crates/a.rs".into(),
                change: Some("+12 −3".into()),
                outcome: None,
                call_id: Some("call_4".into()),
                kind: None,
                reason: None,
            }
        );
        assert_eq!(log.rows[2].change.as_deref(), Some("created"));
        let ran = &log.rows[3];
        assert_eq!(ran.target, "cargo test -p bubblepaw-runner");
        assert_eq!(ran.change, None);
        assert_eq!(ran.outcome.as_deref(), Some("failed 2"));
        assert_eq!(ran.call_id.as_deref(), Some("call_6"));
        assert_eq!(ran.kind, Some(ActionKind::Error));
        assert_eq!(ran.reason.as_deref(), Some("two parser tests failed"));
        assert_eq!(log.rows[4].kind, Some(ActionKind::Decision));
        assert_eq!(log.rows[5].target, "\"which model?\"");
        assert_eq!(log.rows[5].kind, Some(ActionKind::Question));
    }

    #[test]
    fn change_and_outcome_together() {
        let log = parse_actions("Actions:\n- Edited a.rs · +1 -0 · passed 14 [c1]\nOpen: none");
        let row = &log.rows[0];
        assert_eq!(row.change.as_deref(), Some("+1 -0"));
        assert_eq!(row.outcome.as_deref(), Some("passed 14"));
        assert_eq!(row.call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let log = parse_actions(
            "## Actions\n- Patched a.rs [c1]\n- Edited\n- Ran make ! blocker: nope\n- Ran x ·  · b\n- Edited b.rs [c2]\n\nOpen: none",
        );
        assert!(log.present);
        assert_eq!(log.rows.len(), 1);
        assert_eq!(log.rows[0].target, "b.rs");
        assert_eq!(log.malformed.len(), 4);
    }

    #[test]
    fn none_and_missing() {
        let none = parse_actions("# T\nGist.\nActions: none\nOpen: none");
        assert!(none.present && none.rows.is_empty() && none.malformed.is_empty());
        let bold = parse_actions("**Actions:**\n- none\n\nOpen: none");
        assert!(bold.present && bold.rows.is_empty() && bold.malformed.is_empty());
        let missing = parse_actions("# T\nGist.\nOpen: none");
        assert!(!missing.present && missing.rows.is_empty());
    }

    #[test]
    fn section_stops_at_open() {
        let log = parse_actions("Actions:\n- Ran ls · exit 0\nOpen:\n- Edit c.rs next");
        assert_eq!(log.rows.len(), 1);
        assert!(log.malformed.is_empty());
    }

    #[test]
    fn several_outcomes_are_kept_joined() {
        let log = parse_actions(
            "Actions:\n- Ran cargo test · +2 −1 · passed 14 · failed 2 [c1]\n- Ran ls · exit 0 · 3 files\nOpen: none",
        );
        assert!(log.malformed.is_empty(), "{:?}", log.malformed);
        assert_eq!(log.rows[0].change.as_deref(), Some("+2 −1"));
        assert_eq!(log.rows[0].outcome.as_deref(), Some("passed 14 · failed 2"));
        assert_eq!(log.rows[0].call_id.as_deref(), Some("c1"));
        assert_eq!(log.rows[1].change, None);
        assert_eq!(log.rows[1].outcome.as_deref(), Some("exit 0 · 3 files"));
    }

    #[test]
    fn close_synonyms_map_to_listed_verbs() {
        let log = parse_actions(
            "Actions:\n- Rewrote a.rs\n- Modified b.rs\n- Updated c.rs\n- Wrote d.md\n- Executed make\n- Listed crates/x/tests\n- Patched e.rs\n- edited f.rs\nOpen: none",
        );
        let verbs: Vec<_> = log.rows.iter().map(|row| row.verb).collect();
        use ActionVerb::*;
        assert_eq!(verbs, [Edited, Edited, Edited, Created, Ran, Read]);
        assert_eq!(log.rows[3].target, "d.md");
        assert_eq!(log.malformed, ["Patched e.rs", "edited f.rs"]);
    }

    #[test]
    fn gist_is_the_first_line_after_the_title_without_code_markers() {
        let tight = "# Title\nAdded `v3` to crates/x.\nActions: none\nOpen: none";
        assert_eq!(parse_gist(tight).as_deref(), Some("Added v3 to crates/x."));
        let spaced = "\n# Title\n\n  Found the `bug`.  \n\nOpen: none";
        assert_eq!(parse_gist(spaced).as_deref(), Some("Found the bug."));
        assert_eq!(
            parse_gist("# Title\n## Goal\n- item\nx").as_deref(),
            Some("x")
        );
        assert_eq!(parse_gist("# Title\nActions: none\nlater prose"), None);
        assert_eq!(parse_gist("# Title\n## Goal\n- a\n1. b\nOpen: none"), None);
        assert_eq!(parse_gist("No title\nGist"), None);
        assert_eq!(parse_gist("# Title"), None);
    }

    #[test]
    fn gist_is_the_first_prose_line_capped_at_25_words() {
        let late = "# Title\n\n## Goal and constraints\n\n- bullet\nThe `real` goal.\n";
        assert_eq!(parse_gist(late).as_deref(), Some("The real goal."));
        let long = format!("# Title\n{}", vec!["word"; 30].join(" "));
        let gist = parse_gist(&long).expect("gist");
        assert_eq!(gist.split(' ').count(), GIST_WORDS);
    }
}
