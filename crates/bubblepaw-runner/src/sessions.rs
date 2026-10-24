//! Host-owned session naming and opt-in archive retention.
use anyhow::{Result, ensure};
use bubblepaw_core::{Session, Store};

pub const MAX_TITLE_BYTES: usize = 256;

/// An absent title uses the host's ordinary creation label.
pub fn title(value: Option<&str>, default: &str) -> Result<String> {
    let value = value.unwrap_or(default).trim();
    ensure!(!value.is_empty(), "title must not be empty");
    ensure!(
        value.len() <= MAX_TITLE_BYTES,
        "title must be at most {MAX_TITLE_BYTES} bytes"
    );
    Ok(value.to_owned())
}

pub fn rename(store: &Store, session: &str, value: &str) -> Result<Session> {
    store.rename_session(session, title(Some(value), "New session")?)
}

/// A host command: it never becomes conversation history or calls a provider.
pub fn rename_command(text: &str) -> Option<&str> {
    let suffix = text.trim().strip_prefix("/rename")?;
    if !suffix.is_empty() && !suffix.starts_with(char::is_whitespace) {
        return None;
    }
    Some(suffix.trim())
}

/// Startup only, after resource cleanup hooks are registered. Unknown archive
/// dates are retained; active/merely idle chats cannot
/// enter the candidate set. The ordinary subtree deletion also removes blobs,
/// checkpoints and session-owned spill files.
pub fn prune_archives(store: &Store, days: Option<u32>) -> Result<Vec<String>> {
    let instant = chrono::Utc::now();
    let mut policies: std::collections::BTreeMap<u32, std::collections::HashSet<String>> =
        std::collections::BTreeMap::new();
    for root in store.archived_roots_before(&instant.to_rfc3339())? {
        let retention = store
            .session_storage_policy(&root)?
            .map_or(days, |policy| policy.archived_retention_days);
        if let Some(retention) = retention {
            policies.entry(retention).or_default().insert(root);
        }
    }
    let mut removed = Vec::new();
    for (days, eligible) in policies {
        let cutoff = instant
            .checked_sub_signed(chrono::Duration::days(i64::from(days)))
            .ok_or_else(|| {
                anyhow::anyhow!("archived retention is outside the supported date range")
            })?
            .to_rfc3339();
        for root in store.archived_roots_before(&cutoff)? {
            if !eligible.contains(&root) {
                continue;
            }
            if let Err(error) = store.delete_session(&root) {
                // Cleanup callbacks run after the canonical deletion. Report that
                // boundary even when a filesystem error makes startup fail.
                if !store
                    .list_sessions()?
                    .iter()
                    .any(|session| session.id == root)
                {
                    eprintln!(
                        "Archived retention: deleted chat {root}; resource cleanup failed: {error:#}"
                    );
                }
                return Err(error);
            }
            eprintln!(
                "Archived retention: deleted chat {root} (archived more than {days} days ago)"
            );
            removed.push(root);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod storage_policy_tests {
    use super::*;
    use bubblepaw_core::{ContextStrategy, RootStoragePolicy, StoreOptions};

    #[test]
    fn archived_retention_respects_root_policy_and_only_old_chats_use_host_fallback() -> Result<()>
    {
        let data = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let store = Store::open_data_dir(data.path())?;
        let policy = RootStoragePolicy {
            store: StoreOptions {
                archive_after_days: 0,
                ..Default::default()
            },
            archived_retention_days: Some(0),
        };
        let expire = store.create_session_with_policy(
            "expire",
            ContextStrategy::default(),
            workspace.path(),
            policy,
        )?;
        let keep = store.create_session_with_policy(
            "keep",
            ContextStrategy::default(),
            workspace.path(),
            RootStoragePolicy {
                archived_retention_days: None,
                ..policy
            },
        )?;
        let old = store.create_session("old fallback")?;
        for root in [&expire.id, &keep.id, &old.id] {
            assert!(store.archive_chat(root)?);
        }
        let removed = prune_archives(&store, Some(0))?;
        assert!(removed.contains(&expire.id));
        assert!(removed.contains(&old.id));
        assert!(!removed.contains(&keep.id));
        assert_eq!(store.list_sessions()?.len(), 1);
        assert_eq!(store.list_sessions()?[0].id, keep.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_command_accepts_valid_title() {
        assert_eq!(rename_command("/rename Weekly notes"), Some("Weekly notes"));
        assert_eq!(
            rename_command("/rename Rename refactor"),
            Some("Rename refactor")
        );
    }

    #[test]
    fn rename_command_rejects_lookalike_prefix() {
        assert_eq!(rename_command("/renamex"), None);
        assert_eq!(rename_command("/renamed Weekly notes"), None);
        assert_eq!(rename_command("/rename: Weekly notes"), None);
        assert_eq!(rename_command("/renam"), None);
        assert_eq!(rename_command("rename Weekly notes"), None);
    }

    #[test]
    fn rename_command_reports_blank_title() {
        assert_eq!(rename_command("/rename"), Some(""));
        assert_eq!(rename_command("/rename   "), Some(""));
        assert_eq!(rename_command("  /rename  "), Some(""));
        assert!(title(rename_command("/rename   "), "New session").is_err());
    }

    #[test]
    fn rename_command_trims_surrounding_whitespace() {
        assert_eq!(
            rename_command("  /rename   Weekly notes  "),
            Some("Weekly notes")
        );
        assert_eq!(
            rename_command("/rename\tWeekly notes\t"),
            Some("Weekly notes")
        );
        assert_eq!(
            rename_command("/rename\nWeekly notes\n"),
            Some("Weekly notes")
        );
    }

    #[test]
    fn title_accepts_trimmed_value_and_default() {
        assert_eq!(
            title(Some("  Weekly notes  "), "New session").unwrap(),
            "Weekly notes"
        );
        assert_eq!(title(None, "New session").unwrap(), "New session");
        assert_eq!(title(None, "  New session  ").unwrap(), "New session");
        assert!(title(Some("   "), "  New session  ").is_err());
        assert!(title(None, "   ").is_err());
    }

    #[test]
    fn title_enforces_byte_limit() {
        let at_limit = "a".repeat(MAX_TITLE_BYTES);
        assert_eq!(
            title(Some(at_limit.as_str()), "New session").unwrap(),
            at_limit
        );
        let over_limit = "a".repeat(MAX_TITLE_BYTES + 1);
        assert!(title(Some(over_limit.as_str()), "New session").is_err());
        // The limit is in bytes: 129 two-byte characters exceed 256 bytes.
        let multibyte = "é".repeat(MAX_TITLE_BYTES / 2 + 1);
        assert!(multibyte.chars().count() <= MAX_TITLE_BYTES);
        assert!(title(Some(multibyte.as_str()), "New session").is_err());
    }
}
