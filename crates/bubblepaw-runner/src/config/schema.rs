//! Key lookup and live projection over the section schema declared with the settings structs.
use super::{EffectiveConfig, RawSettings};
use bubblepaw_core::config_schema::{Field, LIVE, Node, descend, liveness, resolve};
use std::collections::BTreeSet;
use toml::Value;

/// `providers.<name>.reasoning`: a free-form table in the struct, two known keys here.
pub(super) const REASONING: Node = Node::Table(&[
    Field {
        name: "thinking",
        node: Node::Table(&[Field {
            name: "type",
            node: LIVE,
        }]),
    },
    Field {
        name: "reasoning_effort",
        node: LIVE,
    },
]);

fn root() -> Node {
    Node::Table(RawSettings::FIELDS)
}

/// Whether `path` names a settable key; unknown keys are rejected before deserialization.
pub(super) fn known_path(path: &str) -> bool {
    resolve(RawSettings::FIELDS, &path.split('.').collect::<Vec<_>>()).is_some()
}

/// One setting that differs between two configs.
#[derive(Debug, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    /// A running host applies this key without a restart.
    pub live: bool,
}

pub(super) fn changes(a: &Value, b: &Value) -> Vec<Change> {
    let mut out = Vec::new();
    walk(root(), "", Some(a), Some(b), &mut out);
    out
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.into()
    } else {
        format!("{path}.{name}")
    }
}

type Table = toml::map::Map<String, Value>;

/// `None` when the value is present but not a table (a shape change).
fn table(value: Option<&Value>) -> Option<Option<&Table>> {
    match value {
        None => Some(None),
        Some(Value::Table(table)) => Some(Some(table)),
        Some(_) => None,
    }
}

fn walk(node: Node, path: &str, a: Option<&Value>, b: Option<&Value>, out: &mut Vec<Change>) {
    if a == b {
        return;
    }
    let whole = |out: &mut Vec<Change>, live| {
        out.push(Change {
            path: path.into(),
            live,
        })
    };
    match node {
        Node::Leaf(live) | Node::Free(live) => whole(out, live),
        Node::Table(_) | Node::Flatten(_) | Node::Map(_) => {
            let (Some(a), Some(b)) = (table(a), table(b)) else {
                return whole(out, liveness(node).unwrap_or(false));
            };
            let keys: BTreeSet<&String> = [a, b]
                .into_iter()
                .flatten()
                .flat_map(|t| t.keys())
                .collect();
            for key in keys {
                // A key the schema does not know cannot be applied live.
                let child = descend(node, key).unwrap_or(Node::Leaf(false));
                let (a, b) = (
                    a.and_then(|t| t.get(key.as_str())),
                    b.and_then(|t| t.get(key.as_str())),
                );
                walk(child, &join(path, key), a, b, out);
            }
        }
    }
}

impl EffectiveConfig {
    /// Keys whose value differs from `other`, each with its liveness.
    pub fn changes_from(&self, other: &Self) -> Vec<Change> {
        changes(&self.value, &other.value)
    }

    /// This config's startup-only settings with `live`'s live ones: what a running
    /// host holds once `live` is published. Provenance follows each applied key.
    pub fn with_live_from(&self, live: &Self) -> Self {
        let mut result = self.clone();
        let mut root = Some(result.value.clone());
        project(
            root_fields(),
            "",
            &mut root,
            Some(&live.value),
            live,
            &mut result,
        );
        result.value = root.expect("root table stays present");
        result
    }
}

fn root_fields() -> &'static [Field] {
    RawSettings::FIELDS
}

fn project(
    fields: &'static [Field],
    path: &str,
    base: &mut Option<Value>,
    cand: Option<&Value>,
    live: &EffectiveConfig,
    out: &mut EffectiveConfig,
) {
    let mut table = match base.take() {
        Some(Value::Table(table)) => table,
        _ => Default::default(),
    };
    for field in fields {
        if let Node::Flatten(inner) = field.node {
            let mut whole = Some(Value::Table(std::mem::take(&mut table)));
            project(inner, path, &mut whole, cand, live, out);
            if let Some(Value::Table(back)) = whole {
                table = back;
            }
            continue;
        }
        let child = join(path, field.name);
        let mut slot = table.remove(field.name);
        let theirs = cand.and_then(|c| c.get(field.name));
        match liveness(field.node) {
            Some(false) => {}
            Some(true) => {
                slot = theirs.cloned();
                let prefix = format!("{child}.");
                let owned = |key: &String| *key == child || key.starts_with(&prefix);
                out.sources.retain(|key, _| !owned(key));
                out.bases.retain(|key, _| !owned(key));
                out.sources.extend(
                    live.sources
                        .iter()
                        .filter(|(k, _)| owned(k))
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
                out.bases.extend(
                    live.bases
                        .iter()
                        .filter(|(k, _)| owned(k))
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
            }
            None => match field.node {
                Node::Table(inner) => project(inner, &child, &mut slot, theirs, live, out),
                _ => unreachable!("only tables mix live and startup-only keys"),
            },
        }
        if let Some(value) = slot {
            table.insert(field.name.to_owned(), value);
        }
    }
    *base = Some(Value::Table(table));
}
