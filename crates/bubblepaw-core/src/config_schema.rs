//! Config key schema declared next to the settings structs that deserialize it.
//!
//! A section is defined once with [`config_section!`]: every field names its key
//! node and whether a running host applies it live ([`LIVE`]) or only at startup
//! ([`RESTART`]). The key list, the live answer and the docs check all read these
//! nodes. Not a stable API: it exists so runner crates share one definition.

/// One named key of a table.
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub node: Node,
}

/// What a key path can hold.
#[derive(Clone, Copy, Debug)]
pub enum Node {
    /// A value with no sub-keys; `true` when a running host applies it live.
    Leaf(bool),
    /// Fixed sub-keys.
    Table(&'static [Field]),
    /// A struct flattened into its parent table.
    Flatten(&'static [Field]),
    /// Any non-empty name, each holding the inner node.
    Map(&'static Node),
    /// Any sub-keys at any depth, all with this liveness.
    Free(bool),
}

pub const LIVE: Node = Node::Leaf(true);
pub const RESTART: Node = Node::Leaf(false);

/// Define a settings struct and its key schema from one field list.
#[macro_export]
macro_rules! config_section {
    ($(#[$meta:meta])* $vis:vis struct $name:ident {
        $($(#[$fmeta:meta])* $fvis:vis $field:ident : $ty:ty => $node:expr),* $(,)?
    }) => {
        $(#[$meta])* $vis struct $name { $($(#[$fmeta])* $fvis $field: $ty),* }
        impl $name {
            pub const FIELDS: &'static [$crate::config_schema::Field] = &[
                $($crate::config_schema::Field { name: stringify!($field), node: $node }),*
            ];
        }
    };
}

fn child(fields: &'static [Field], name: &str) -> Option<Node> {
    fields.iter().find_map(|field| match field.node {
        Node::Flatten(inner) => child(inner, name),
        node => (field.name == name).then_some(node),
    })
}

/// The sub-node a key leads to, or `None` for an unknown key. `Free` and `Leaf`
/// nodes have no children. Empty names never match a map.
pub fn descend(node: Node, name: &str) -> Option<Node> {
    match node {
        Node::Table(fields) | Node::Flatten(fields) => child(fields, name),
        Node::Map(inner) => (!name.is_empty()).then_some(*inner),
        Node::Free(_) => (!name.is_empty()).then_some(node),
        Node::Leaf(_) => None,
    }
}

/// Resolve a dotted path from the root table; `None` when any part is unknown.
pub fn resolve(root: &'static [Field], parts: &[&str]) -> Option<Node> {
    parts
        .iter()
        .try_fold(Node::Table(root), |node, part| descend(node, part))
}

/// Every key path under `node` with its liveness. Map names show as `<name>`,
/// free-form tables as `<key>`; a table that holds no leaf shows as itself.
pub fn leaves(node: Node, path: &str, out: &mut Vec<(String, bool)>) {
    let join = |name: &str| match path.is_empty() {
        true => name.to_owned(),
        false => format!("{path}.{name}"),
    };
    match node {
        Node::Leaf(live) => out.push((path.to_owned(), live)),
        Node::Table(fields) | Node::Flatten(fields) => {
            for field in fields {
                match field.node {
                    Node::Flatten(_) => leaves(field.node, path, out),
                    node => leaves(node, &join(field.name), out),
                }
            }
        }
        Node::Map(inner) => leaves(*inner, &join("<name>"), out),
        Node::Free(live) => out.push((join("<key>"), live)),
    }
}

/// `Some(true)` when every key under `node` is live, `Some(false)` when none is,
/// `None` for a mix.
pub fn liveness(node: Node) -> Option<bool> {
    let mut all = Vec::new();
    leaves(node, "", &mut all);
    let live = all.iter().filter(|(_, live)| *live).count();
    match (live, all.len()) {
        (n, total) if n == total => Some(true),
        (0, _) => Some(false),
        _ => None,
    }
}
