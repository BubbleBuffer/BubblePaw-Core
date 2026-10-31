//! Named toolsets: which tools the main chat can use. `agent.toolset` picks one live;
//! `[toolsets.<name>] tools = [...]` defines more beside the built-in `all` and `read-only`.
//! The published registry stays whole for children; the main chat sees `apply`'s view.
use crate::config::Settings;
use anyhow::{Result, ensure};
use bubblepaw_core::{
    ExecutionMode, ToolRegistry, config_schema::LIVE, config_section, tool_definition_summary,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Every tool.
pub const ALL: &str = "all";
/// The tools declared read-only, as `approvals.read_only` filters them.
pub const READ_ONLY: &str = "read-only";
const CATEGORY: &str = "category:";
const MCP_CATEGORY: &str = "mcp:";
/// The built-in categories in display order. An MCP server's tools form the category
/// `mcp:<server>`; a tool outside every list belongs to `other`.
pub const CATEGORIES: &[(&str, &[&str])] = &[
    ("files", &["list", "read", "search", "edit", "write"]),
    ("commands", &["exec", "poll", "wait", "cancel", "jobs"]),
    ("web", &["web_search", "web_fetch"]),
    ("agents", &["agent", "agent_send", "agent_status"]),
    (
        "history",
        &["history_chapters", "history_search", "history_read"],
    ),
    (
        "other",
        &[
            "js",
            "question",
            "todo",
            "reload",
            "environments",
            "tool_search",
            "call_tool",
        ],
    ),
];
/// Native-mode discovery adapters: registered per run, always read-only.
const DISCOVERY: [&str; 2] = ["tool_search", "call_tool"];

config_section! {
    /// A named set of tools: exact tool names and `category:<id>` entries.
    #[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
    #[serde(default, deny_unknown_fields)]
    pub struct Toolset {
        pub tools: Vec<String> => LIVE,
    }
}

/// What the settings hub publishes: the main chat's toolset and the user-defined ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolsetSettings {
    pub main: String,
    pub sets: BTreeMap<String, Toolset>,
}
impl From<&Settings> for ToolsetSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            main: settings.agent.toolset.clone(),
            sets: settings.toolsets.clone(),
        }
    }
}

fn valid_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Check the toolset definitions and that the main chat's choice exists.
pub fn validate(sets: &BTreeMap<String, Toolset>, main: &str) -> Result<()> {
    for (name, set) in sets {
        ensure!(
            valid_name(name),
            "toolsets.{name}: use letters, numbers, - and _, starting with a letter or number"
        );
        ensure!(
            name != ALL && name != READ_ONLY,
            "toolsets.{name} is built in and cannot be redefined"
        );
        for entry in &set.tools {
            match entry.strip_prefix(CATEGORY) {
                Some(id) => ensure!(
                    CATEGORIES.iter().any(|(known, _)| *known == id)
                        || id
                            .strip_prefix(MCP_CATEGORY)
                            .is_some_and(|server| !server.is_empty()),
                    "toolsets.{name}.tools: unknown category {id}"
                ),
                None => ensure!(
                    !entry.is_empty() && !entry.chars().any(char::is_whitespace),
                    "toolsets.{name}.tools entries are tool names or category:<id>"
                ),
            }
        }
    }
    ensure!(
        main == ALL || main == READ_ONLY || sets.contains_key(main),
        "agent.toolset names an unknown toolset: {main}"
    );
    Ok(())
}

/// The MCP server a tool came from, by the registry's record of its original name.
fn server_of(registry: &ToolRegistry, name: &str) -> Option<String> {
    registry.tool_server(name).map(str::to_owned).or_else(|| {
        name.strip_prefix("mcp__")
            .and_then(|rest| rest.split("__").next())
            .map(str::to_owned)
    })
}

/// A tool's category id: a built-in one, `mcp:<server>`, or `other`.
pub fn category_of(registry: &ToolRegistry, name: &str) -> String {
    if let Some(server) = server_of(registry, name) {
        return format!("{MCP_CATEGORY}{server}");
    }
    CATEGORIES
        .iter()
        .find(|(_, names)| names.contains(&name))
        .map_or("other", |(id, _)| id)
        .to_owned()
}

/// One tool's identity for the toolset checks: registered tools plus the discovery adapters.
fn members(registry: &ToolRegistry) -> Vec<(String, bool)> {
    let mut all: Vec<_> = registry
        .entries()
        .into_iter()
        .map(|tool| (tool.name, tool.read_only))
        .collect();
    for name in DISCOVERY {
        if !all.iter().any(|(known, _)| known == name) {
            all.push((name.to_owned(), true));
        }
    }
    all
}

/// The view of the whole published registry the main chat runs with. A tool the toolset
/// leaves out is removed with a reason, so a call to it says so. `category:` entries
/// match by the tool's category at each publication, so tools added later are included.
pub fn apply(full: &ToolRegistry, settings: &ToolsetSettings) -> ToolRegistry {
    let custom = match settings.main.as_str() {
        ALL => return full.clone(),
        READ_ONLY => None,
        name => match settings.sets.get(name) {
            Some(set) => Some(set),
            None => return full.clone(),
        },
    };
    let mut view = full.clone();
    for (name, read_only) in members(full) {
        let allowed = match custom {
            None => read_only,
            Some(set) => set
                .tools
                .iter()
                .any(|entry| match entry.strip_prefix(CATEGORY) {
                    Some(id) => category_of(full, &name) == id,
                    None => *entry == name,
                }),
        };
        if !allowed {
            view.remove(&name, format!("not in the {} toolset", settings.main));
        }
    }
    view
}

/// One tool as `tools.list` reports it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    /// `files`, `commands`, `web`, `agents`, `history`, `other` or `mcp:<server>`.
    pub category: String,
    /// The first sentence of the tool's description.
    pub description: String,
    /// `builtin` or `mcp`.
    pub source: String,
    /// The MCP server for an `mcp` tool.
    pub server: Option<String>,
    /// Whether the built-in `read-only` toolset includes it.
    pub read_only: bool,
}

/// Every tool of the whole published registry, built-in categories first, then MCP servers.
pub fn list(full: &ToolRegistry) -> Vec<ToolInfo> {
    let native = full.with_execution_mode(ExecutionMode::Native);
    let mut seen = std::collections::BTreeSet::new();
    let mut tools: Vec<_> = full
        .entries()
        .into_iter()
        .chain(native.entries())
        .filter(|tool| seen.insert(tool.name.clone()))
        .map(|tool| {
            let server = server_of(full, &tool.name);
            ToolInfo {
                category: category_of(full, &tool.name),
                description: tool_definition_summary(&tool.description),
                source: if server.is_some() { "mcp" } else { "builtin" }.into(),
                server,
                read_only: tool.read_only,
                name: tool.name,
            }
        })
        .collect();
    let rank = |category: &str| {
        CATEGORIES
            .iter()
            .position(|(id, _)| *id == category)
            .unwrap_or(CATEGORIES.len())
    };
    tools.sort_by(|a, b| {
        (rank(&a.category), &a.category, &a.name).cmp(&(rank(&b.category), &b.category, &b.name))
    });
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sets(entries: &[(&str, &[&str])]) -> BTreeMap<String, Toolset> {
        entries
            .iter()
            .map(|(name, tools)| {
                (
                    (*name).to_owned(),
                    Toolset {
                        tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn validation_refuses_built_in_names_bad_entries_and_unknown_main() {
        assert!(validate(&sets(&[("no-web", &["category:files", "read"])]), "no-web").is_ok());
        assert!(validate(&sets(&[("mix", &["category:mcp:github"])]), "all").is_ok());
        for (bad, why) in [
            (sets(&[("all", &[])]), "built in"),
            (sets(&[("read-only", &["read"])]), "built in"),
            (sets(&[("has space", &[])]), "letters"),
            (sets(&[("-lead", &[])]), "letters"),
            (sets(&[("x", &["category:nope"])]), "unknown category"),
            (sets(&[("x", &["category:mcp:"])]), "unknown category"),
            (sets(&[("x", &["two words"])]), "tool names"),
        ] {
            let error = validate(&bad, "all").unwrap_err().to_string();
            assert!(error.contains(why), "{error}");
        }
        assert!(validate(&sets(&[]), "missing").is_err());
        assert!(validate(&sets(&[]), "read-only").is_ok());
    }
}
