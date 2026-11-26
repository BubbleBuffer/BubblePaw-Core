//! Step-by-step upgrade of an older store file to the current schema version.
//! Step `N` turns version `N` into `N + 1`; each step and its version bump commit
//! in one transaction, so a crash leaves a consistent file at an intermediate
//! version and the next open continues from there.
use super::schema::StoreSchemaError;
use anyhow::Result;
use rusqlite::{Connection, Transaction, TransactionBehavior};

/// Upgrade body for one version. It runs inside the transaction that also bumps
/// `user_version`, so a failure rolls both back.
pub(super) type StepFn = fn(&Transaction) -> Result<()>;

/// Turns version `from` into `from + 1`.
pub(super) struct Step {
    pub(super) from: i64,
    pub(super) run: StepFn,
}

/// Upgrade steps for the chat store. A schema change adds one here and raises
/// `CHAT_VERSION` in the same change.
pub(super) const CHAT_STEPS: &[Step] = &[
    Step {
        from: 12,
        run: message_content_list,
    },
    Step {
        from: 13,
        run: message_image_blocks,
    },
];
/// Version 13 stores new message rows with `content` as a list of blocks. Old
/// rows keep the string form and stay readable, so no data changes; only the
/// version bump matters, because a program that predates the list form must
/// refuse a store that may hold such rows.
fn message_content_list(_tx: &Transaction) -> Result<()> {
    Ok(())
}
/// Version 14 stores images as `image` blocks in the content list. Rows that
/// carry the older text envelope stay as they are and are read as image blocks,
/// so only the version bump matters: a program that predates the block must
/// refuse a store that may hold it.
fn message_image_blocks(_tx: &Transaction) -> Result<()> {
    Ok(())
}
/// Upgrade steps for `catalog.db`.
pub(super) const CATALOG_STEPS: &[Step] = &[];
/// Upgrade steps for `serving.db`.
pub(super) const SERVING_STEPS: &[Step] = &[];

fn chain_exists(steps: &[Step], found: i64, expected: i64) -> bool {
    (found..expected).all(|from| steps.iter().any(|step| step.from == from))
}

fn version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Bring an existing file to `expected`, or refuse with [`StoreSchemaError`]
/// without touching it. A version already at `expected` costs one pragma read.
pub(super) fn upgrade(
    conn: &mut Connection,
    store: &'static str,
    expected: i64,
    steps: &[Step],
) -> Result<()> {
    let found = version(conn)?;
    if found == expected {
        return Ok(());
    }
    if found > expected || !chain_exists(steps, found, expected) {
        return StoreSchemaError::ensure(store, found, expected);
    }
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    loop {
        // The write lock is taken before the version is read, so a second
        // process opening the same file waits, then sees the upgrade done.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = version(&tx)?;
        if current == expected {
            return Ok(());
        }
        if current > expected || !chain_exists(steps, current, expected) {
            return StoreSchemaError::ensure(store, current, expected);
        }
        let step = steps
            .iter()
            .find(|step| step.from == current)
            .expect("chain_exists found it");
        (step.run)(&tx)?;
        tx.execute_batch(&format!("PRAGMA user_version={};", current + 1))?;
        tx.commit()?;
    }
}

/// A read-only open cannot upgrade: an older version with a chain asks for a
/// writable open; everything else is checked as before.
pub(super) fn ensure_read_only(
    store: &'static str,
    found: i64,
    expected: i64,
    steps: &[Step],
) -> Result<()> {
    if found < expected && chain_exists(steps, found, expected) {
        return Err(anyhow::Error::new(StoreSchemaError {
            store,
            found,
            expected,
            needs_writable_open: true,
        }));
    }
    StoreSchemaError::ensure(store, found, expected)
}

#[cfg(test)]
#[path = "store_upgrade_tests.rs"]
mod tests;
