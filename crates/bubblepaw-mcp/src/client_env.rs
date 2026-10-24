//! Request-scoped environment supplied by the client that starts a host.
//! The snapshot is kept by one host for its whole life and is never serialized.
use std::{collections::BTreeMap, fmt};

/// A private snapshot of client environment values. Debug prints names only.
/// Names match the platform's rules: case-insensitive on Windows, as in `std::env`.
#[derive(Clone, Default)]
pub struct McpClientEnv {
    values: BTreeMap<String, String>,
}

impl McpClientEnv {
    /// Build a snapshot from the client's already-filtered variables. On Windows a
    /// later name that differs only in case replaces the earlier one, as `std::env` does.
    pub fn new<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut values = BTreeMap::new();
        for (name, value) in pairs {
            let name = name.into();
            if cfg!(windows) {
                values.retain(|existing: &String, _| !same_name(existing, &name));
            }
            values.insert(name, value.into());
        }
        Self { values }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        if cfg!(windows) {
            self.values
                .iter()
                .find(|(existing, _)| same_name(existing, name))
                .map(|(_, value)| value.as_str())
        } else {
            self.values.get(name).map(String::as_str)
        }
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

/// Windows environment names are case-insensitive; other platforms compare exactly.
fn same_name(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

impl fmt::Debug for McpClientEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpClientEnv")
            .field("names", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_lookup_follows_platform_env_case_rules() {
        let env = McpClientEnv::new([("Path", "first"), ("TOKEN", "second")]);
        // Exact spelling always finds its value.
        assert_eq!(env.get("Path"), Some("first"));
        assert_eq!(env.get("TOKEN"), Some("second"));
        if cfg!(windows) {
            assert_eq!(env.get("PATH"), Some("first"));
            assert_eq!(env.get("token"), Some("second"));
        } else {
            assert_eq!(env.get("PATH"), None);
            assert_eq!(env.get("token"), None);
        }
    }

    #[test]
    fn windows_duplicate_names_keep_the_later_value_only() {
        let env = McpClientEnv::new([("Token", "earlier"), ("TOKEN", "later")]);
        if cfg!(windows) {
            assert_eq!(env.iter().count(), 1);
            assert_eq!(env.get("token"), Some("later"));
        } else {
            assert_eq!(env.iter().count(), 2);
            assert_eq!(env.get("Token"), Some("earlier"));
        }
    }
}
