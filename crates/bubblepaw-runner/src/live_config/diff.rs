//! Bounded reload reporting for accepted configuration and MCP changes.
use super::*;

pub(super) fn config_diff(old: &EffectiveConfig, new: &EffectiveConfig) -> Vec<String> {
    let mut diff = Vec::new();
    for section in [
        "model",
        "providers",
        "roles",
        "web",
        "agents",
        "exec",
        "mcp",
        "skills",
        "toolsets",
    ] {
        if old.value(section) != new.value(section) {
            diff.push(format!("{section} updated"));
        }
    }
    if old.value("approvals") != new.value("approvals") {
        let before = old.settings().expect("accepted config").approvals;
        let after = new.settings().expect("validated config").approvals;
        if before.confirm != after.confirm {
            diff.push(format!(
                "approval default: {} → {}",
                before.confirm, after.confirm
            ));
        }
        let names: std::collections::BTreeSet<_> =
            before.tools.keys().chain(after.tools.keys()).collect();
        for name in names {
            let from = before
                .tools
                .get(name)
                .map(String::as_str)
                .unwrap_or("default");
            let to = after
                .tools
                .get(name)
                .map(String::as_str)
                .unwrap_or("default");
            if from != to {
                diff.push(format!("approval for {name}: {from} → {to}"));
            }
        }
        if before.deny != after.deny {
            diff.push(format!(
                "approval deny rules: {} → {}",
                before.deny.join(", "),
                after.deny.join(", ")
            ));
        }
        if before.read_only != after.read_only {
            diff.push("approval read-only setting updated".into());
        }
        if before.hardline != after.hardline {
            let label = |on: bool| if on { "on" } else { "off" };
            diff.push(format!(
                "approval hardline floor: {} → {}",
                label(before.hardline),
                label(after.hardline)
            ));
        }
    }
    let toolset = |config: &EffectiveConfig| {
        config
            .value("agent.toolset")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    if toolset(old) != toolset(new) {
        diff.push(format!("main toolset: {} → {}", toolset(old), toolset(new)));
    }
    context_diff(
        "context",
        old.value("context"),
        new.value("context"),
        &mut diff,
    );
    for section in ["storage", "extensions"] {
        if old.value(section) != new.value(section) {
            diff.push(format!("{section} updated (restart needed)"));
        }
    }
    diff
}
// Report frozen leaves separately so a mixed edit never labels live controls
// as restart-only or implies that the rest of context now applies live.
fn context_diff(
    path: &str,
    old: Option<&toml::Value>,
    new: Option<&toml::Value>,
    diff: &mut Vec<String>,
) {
    if old == new {
        return;
    }
    if let (Some(toml::Value::Table(old)), Some(toml::Value::Table(new))) = (old, new) {
        let names: std::collections::BTreeSet<_> = old.keys().chain(new.keys()).collect();
        for name in names {
            context_diff(
                &format!("{path}.{name}"),
                old.get(name),
                new.get(name),
                diff,
            );
        }
    } else {
        let live = matches!(
            path,
            "context.window_tokens" | "context.compact_threshold_tokens" | "context.auto_summarize"
        );
        diff.push(format!(
            "{path} updated{}",
            if live { "" } else { " (restart needed)" }
        ));
    }
}
pub(super) fn add_mcp_diff(report: &mut ReloadReport, diff: &bubblepaw_mcp::McpDiff) {
    report
        .changed
        .extend(diff.added.iter().map(|name| format!("+ {name}")));
    report
        .changed
        .extend(diff.removed.iter().map(|name| format!("− {name}")));
    report
        .changed
        .extend(diff.changed.iter().map(|name| format!("~ {name}")));
    report.changed.extend(
        diff.started
            .iter()
            .map(|name| format!("server {name} started")),
    );
    report.changed.extend(
        diff.stopped
            .iter()
            .map(|name| format!("server {name} stopped: config removed or disabled")),
    );
    report.changed.extend(
        diff.restarted
            .iter()
            .map(|name| format!("server {name} restarted")),
    );
    report.errors.extend(diff.warnings.iter().cloned());
    report.changed.extend(
        diff.filtered
            .iter()
            .map(|name| format!("server {name} tool visibility settings updated")),
    );
}
