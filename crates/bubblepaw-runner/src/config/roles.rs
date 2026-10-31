//! Role references: `main`, other roles and `<provider>/<model>` routes. A
//! failure is a role error, which `HostConfig::load` reports as such.
use super::{RoleEntry, RoleTable};
use anyhow::{Context, Result, ensure};
use bubblepaw_core::config_schema::{Field, Node};
use std::collections::BTreeMap;

pub(super) fn resolve_role(
    name: &str,
    roles: &BTreeMap<String, RoleEntry>,
    main: &str,
    stack: &mut Vec<String>,
) -> Result<Vec<String>> {
    resolve(name, roles, main, stack).map_err(crate::host_config::role_error)
}

fn resolve(
    name: &str,
    roles: &BTreeMap<String, RoleEntry>,
    main: &str,
    stack: &mut Vec<String>,
) -> Result<Vec<String>> {
    ensure!(
        !stack.iter().any(|entry| entry == name),
        "Circular role reference: roles.{name}"
    );
    stack.push(name.into());
    let entry = roles
        .get(name)
        .with_context(|| format!("Unknown role reference: roles.{name}"))?;
    let mut models = Vec::new();
    for model in entry.models() {
        let (reference, llm) = model
            .strip_prefix("llm/")
            .map_or((model.as_str(), false), |reference| (reference, true));
        let resolved = if reference == "main" {
            vec![main.to_owned()]
        } else if roles.contains_key(reference) {
            resolve(reference, roles, main, stack)?
        } else {
            ensure!(
                reference
                    .split_once('/')
                    .is_some_and(
                        |(provider, model)| !provider.trim().is_empty() && !model.trim().is_empty()
                    ),
                "Invalid model reference in roles.{name}"
            );
            vec![reference.into()]
        };
        ensure!(
            !llm || matches!(name, "decider" | "approver"),
            "LLM decision references are only valid in decision roles: roles.{name}"
        );
        for value in resolved {
            let value = if llm && !value.starts_with("llm/") {
                format!("llm/{value}")
            } else {
                value
            };
            if !models.contains(&value) {
                models.push(value);
            }
        }
    }
    stack.pop();
    Ok(models)
}

/// The four role names; each holds a model list or a table (see `RoleTable`).
pub(super) const ROLE_FIELDS: &[Field] = &[
    Field {
        name: "summary",
        node: Node::Table(RoleTable::FIELDS),
    },
    Field {
        name: "chapter",
        node: Node::Table(RoleTable::FIELDS),
    },
    Field {
        name: "decider",
        node: Node::Table(RoleTable::FIELDS),
    },
    Field {
        name: "approver",
        node: Node::Table(RoleTable::FIELDS),
    },
];
