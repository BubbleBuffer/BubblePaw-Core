//! Assignment files: Markdown with TOML front matter between `+++` lines.
use super::read_first::ReadFirst;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where an attempt runs. The dispatcher always states one of the two.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Route {
    /// A fixed `provider/model`, recorded as `named` and never explored.
    Model(String),
    /// A configured class whose default runs unless the attempt is explored.
    Class(String),
}

/// Why this assignment follows an earlier failed resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FollowUp {
    Reframe,
    Escalate,
}
impl FollowUp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reframe => "reframe",
            Self::Escalate => "escalate",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub category: String,
    pub tags: Vec<String>,
    pub checks: Vec<String>,
    pub rework: u32,
    pub route: Route,
    /// Ledger id of the failed attempt this follows, with its reason.
    pub parent: Option<(String, FollowUp)>,
    /// Workspace files read before the first model call; empty when none are named.
    pub read_first: Vec<ReadFirst>,
    /// The Markdown body given to the worker.
    pub body: String,
    /// SHA-256 of the whole file, hex.
    pub hash: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Front {
    category: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default = "default_rework")]
    rework: u32,
    route: RawRoute,
    parent: Option<String>,
    reason: Option<String>,
    #[serde(default)]
    read_first: Vec<String>,
}
fn default_rework() -> u32 {
    DEFAULT_REWORK
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    model: Option<String>,
    class: Option<String>,
}

/// Upper bound on resumes, so a typo cannot loop for hours.
pub const MAX_REWORK: u32 = 8;
/// Resumes with failing check output when the assignment does not say.
pub const DEFAULT_REWORK: u32 = 1;
/// The controlled category list; later automatic routing keys on it.
pub const CATEGORIES: &[&str] = &[
    "implement",
    "fix",
    "refactor",
    "test-write",
    "test-run",
    "research",
    "review",
    "docs",
    "migration",
    "ops",
];

impl Assignment {
    /// Parse and validate, including the category against [`CATEGORIES`].
    pub fn parse(text: &str) -> Result<Self> {
        let hash = bubblepaw_core::hex_lower(Sha256::digest(text.as_bytes()));
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = text.split_inclusive('\n');
        let first = lines.next().unwrap_or_default();
        ensure!(
            first.trim_end() == "+++",
            "assignment must start with a `+++` TOML front matter line"
        );
        let mut front = String::new();
        let mut closed = false;
        for line in lines.by_ref() {
            if line.trim_end() == "+++" {
                closed = true;
                break;
            }
            front.push_str(line);
        }
        ensure!(closed, "assignment front matter is not closed with `+++`");
        let body: String = lines.collect();
        let body = body.trim().to_owned();
        ensure!(!body.is_empty(), "assignment body is empty");
        let front: Front = toml::from_str(&front).context("invalid assignment front matter")?;
        let category = front.category.trim().to_owned();
        ensure!(
            CATEGORIES.contains(&category.as_str()),
            "unknown assignment category `{category}`; known: {}",
            CATEGORIES.join(", ")
        );
        ensure!(
            front.tags.iter().all(|tag| !tag.trim().is_empty()),
            "assignment tags must be nonempty"
        );
        ensure!(
            front.checks.iter().all(|check| !check.trim().is_empty()),
            "assignment checks must be nonempty commands"
        );
        ensure!(
            front.rework <= MAX_REWORK,
            "assignment rework must be at most {MAX_REWORK}"
        );
        let route = match (front.route.model, front.route.class) {
            (Some(model), None) => {
                let model = model.trim().to_owned();
                ensure!(
                    model
                        .split_once('/')
                        .is_some_and(|(p, m)| !p.is_empty() && !m.is_empty()),
                    "route.model must be `provider/model`"
                );
                Route::Model(model)
            }
            (None, Some(class)) => {
                let class = class.trim().to_owned();
                ensure!(!class.is_empty(), "route.class must not be empty");
                Route::Class(class)
            }
            _ => bail!("route must set exactly one of `model` or `class`"),
        };
        let parent = match (front.parent, front.reason) {
            (None, None) => None,
            (Some(parent), Some(reason)) => {
                let parent = parent.trim().to_owned();
                ensure!(!parent.is_empty(), "parent must not be empty");
                let reason = match reason.as_str() {
                    "reframe" => FollowUp::Reframe,
                    "escalate" => FollowUp::Escalate,
                    other => bail!("reason must be `reframe` or `escalate`, not `{other}`"),
                };
                Some((parent, reason))
            }
            _ => bail!("`parent` and `reason` must be given together"),
        };
        // Syntax only here; the files are read when the run starts, in `read_first::section`.
        let read_first = front
            .read_first
            .iter()
            .map(String::as_str)
            .map(ReadFirst::parse)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            category,
            tags: front.tags,
            checks: front.checks,
            rework: front.rework,
            route,
            parent,
            read_first,
            body,
            hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Assignment> {
        Assignment::parse(text)
    }

    #[test]
    fn parses_complete_front_matter() {
        let a = parse(
            "+++\ncategory = \"fix\"\ntags = [\"rust\"]\nchecks = [\"cargo test\"]\nrework = 2\nroute = { class = \"small\" }\nparent = \"abc\"\nreason = \"reframe\"\n+++\n\nFix the bug.\n",
        )
        .unwrap();
        assert_eq!(a.category, "fix");
        assert_eq!(a.tags, ["rust"]);
        assert_eq!(a.checks, ["cargo test"]);
        assert_eq!(a.rework, 2);
        assert_eq!(a.route, Route::Class("small".into()));
        assert_eq!(a.parent, Some(("abc".into(), FollowUp::Reframe)));
        assert_eq!(a.body, "Fix the bug.");
        assert_eq!(a.hash.len(), 64);
    }

    #[test]
    fn parses_read_first_with_and_without_ranges() {
        let a = parse(
            "+++\ncategory = \"fix\"\nroute = { class = \"small\" }\nread_first = [\"docs/x.md\", \"crates/a/src/lib.rs:10-80\"]\n+++\nFix it.\n",
        )
        .unwrap();
        assert_eq!(a.read_first.len(), 2);
        assert_eq!(a.read_first[0].path, "docs/x.md");
        assert_eq!(a.read_first[0].lines, None);
        assert_eq!(a.read_first[1].entry, "crates/a/src/lib.rs:10-80");
        assert_eq!(a.read_first[1].path, "crates/a/src/lib.rs");
        assert_eq!(a.read_first[1].lines, Some((10, 80)));
        let none = parse("+++\ncategory = \"fix\"\nroute = { class = \"small\" }\n+++\nFix it.\n")
            .unwrap();
        assert!(none.read_first.is_empty());
    }

    #[test]
    fn rejects_malformed_read_first_entries() {
        let route = "route = { class = \"small\" }";
        for entry in ["/etc/passwd", "a.md:5-3", "a.md:x", "a.md:0-2", ""] {
            let text =
                format!("+++\ncategory = \"fix\"\n{route}\nread_first = [\"{entry}\"]\n+++\nx");
            assert!(parse(&text).is_err(), "{entry} should be rejected");
        }
    }

    #[test]
    fn defaults_rework_and_accepts_named_model() {
        let a = parse("+++\r\ncategory = \"research\"\r\nroute = { model = \"opencode/deepseek-v4.1-flash\" }\r\n+++\r\nLook.\r\n").unwrap();
        assert_eq!(a.rework, 1);
        assert!(a.checks.is_empty());
        assert_eq!(a.route, Route::Model("opencode/deepseek-v4.1-flash".into()));
        assert_eq!(a.parent, None);
    }

    #[test]
    fn rejects_malformed_input() {
        let route = "route = { class = \"small\" }";
        let cases = [
            ("no front matter", "Fix it.".to_owned()),
            ("unclosed", format!("+++\ncategory = \"fix\"\n{route}\nFix")),
            (
                "empty body",
                format!("+++\ncategory = \"fix\"\n{route}\n+++\n  \n"),
            ),
            ("bad toml", "+++\ncategory = fix\n+++\nx".to_owned()),
            (
                "unknown category",
                format!("+++\ncategory = \"dance\"\n{route}\n+++\nx"),
            ),
            ("missing category", format!("+++\n{route}\n+++\nx")),
            (
                "missing route",
                "+++\ncategory = \"fix\"\n+++\nx".to_owned(),
            ),
            (
                "both routes",
                "+++\ncategory = \"fix\"\nroute = { class = \"small\", model = \"a/b\" }\n+++\nx"
                    .to_owned(),
            ),
            (
                "empty route",
                "+++\ncategory = \"fix\"\nroute = {}\n+++\nx".to_owned(),
            ),
            (
                "model without provider",
                "+++\ncategory = \"fix\"\nroute = { model = \"flash\" }\n+++\nx".to_owned(),
            ),
            (
                "unknown key",
                format!("+++\ncategory = \"fix\"\nkind = \"fix\"\n{route}\n+++\nx"),
            ),
            (
                "escalate ladder",
                format!("+++\ncategory = \"fix\"\nescalate = [\"a/b\"]\n{route}\n+++\nx"),
            ),
            (
                "parent without reason",
                format!("+++\ncategory = \"fix\"\n{route}\nparent = \"p\"\n+++\nx"),
            ),
            (
                "bad reason",
                format!(
                    "+++\ncategory = \"fix\"\n{route}\nparent = \"p\"\nreason = \"retry\"\n+++\nx"
                ),
            ),
            (
                "empty check",
                format!("+++\ncategory = \"fix\"\n{route}\nchecks = [\" \"]\n+++\nx"),
            ),
            (
                "negative rework",
                format!("+++\ncategory = \"fix\"\n{route}\nrework = -1\n+++\nx"),
            ),
            (
                "huge rework",
                format!("+++\ncategory = \"fix\"\n{route}\nrework = 99\n+++\nx"),
            ),
        ];
        for (name, text) in cases {
            assert!(parse(&text).is_err(), "{name} should be rejected");
        }
    }
}
