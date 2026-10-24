//! One-time import of legacy per-workspace stores into the central store.
mod support;

use anyhow::Result;
use bubblepaw_core::{
    ContextStrategy, LegacyImportStatus, QuotaReading, QuotaSource, RequestUsage, Role, Store,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use support::uuid_message;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

fn reading(account: &str, percent: f64, observed: &str) -> QuotaReading {
    QuotaReading {
        provider: "p".into(),
        account: account.into(),
        window: "weekly".into(),
        used_percent: percent,
        resets_at: Some("2026-10-25T00:00:00Z".into()),
        observed_at: observed.into(),
        source: QuotaSource::ResponseHeaders,
        balance: None,
    }
}

/// Every file below `root` with its SHA-256, to prove sources stay unchanged.
fn digest_tree(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    format!("{:x}", Sha256::digest(&bytes)),
                );
            }
        }
    }
    files
}

struct Legacy {
    id: String,
    child: String,
    archived: String,
    workspace: PathBuf,
}

struct Lab {
    _root: tempfile::TempDir,
    base: PathBuf,
    a: Legacy,
    b: Legacy,
}

impl Lab {
    /// Two legacy stores beside the central `store` directory: A binds its
    /// workspace store-wide (the old layout), B binds per root chat.
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let base = root.path().join("BubblePaw-base");
        let mut legacy = Vec::new();
        for (name, per_root) in [(A, false), (B, true)] {
            let workspace = root.path().join(format!("workspace-{}", &name[..4]));
            std::fs::create_dir_all(&workspace)?;
            let store = Store::open_data_dir(base.join(name))?;
            let session = if per_root {
                store.create_session_with_workspace(format!("chat {}", &name[..4]), &workspace)?
            } else {
                store.bind_workspace(&workspace)?;
                store.create_session(format!("chat {}", &name[..4]))?
            };
            let child = store.create_session_with_parent(
                "child",
                ContextStrategy::Lcm,
                Some(&session.id),
            )?;
            store.append_batch(&[
                uuid_message(
                    &session.id,
                    Role::User,
                    &format!("question in {}", &name[..4]),
                ),
                uuid_message(&session.id, Role::Tool, &"x".repeat(6000)),
                uuid_message(&child.id, Role::Assistant, "child reply"),
            ])?;
            let mut usage = RequestUsage::new("p", "m");
            usage.session_id.clone_from(&session.id);
            usage.request_id = format!("request-{}", &name[..4]);
            store.queue_request_usage(usage)?;
            store.flush_request_usage(&session.id)?;
            let archived = if per_root {
                store.create_session_with_workspace("idle", &workspace)?
            } else {
                store.create_session("idle")?
            };
            store.append_batch(&[uuid_message(&archived.id, Role::User, "archived question")])?;
            assert!(store.archive_chat(&archived.id)?);
            let (own, shared) = if per_root {
                ("only-b", ["2026-10-20T02:00:00Z"].as_slice())
            } else {
                (
                    "only-a",
                    ["2026-10-20T01:00:00Z", "2026-10-20T03:00:00Z"].as_slice(),
                )
            };
            store.record_quota(&reading(own, 10.0, "2026-10-20T00:00:00Z"))?;
            store.record_quota(&reading(own, 20.0, "2026-10-20T00:30:00Z"))?;
            for (offset, observed) in shared.iter().enumerate() {
                let percent = if per_root {
                    60.0
                } else {
                    50.0 + 20.0 * offset as f64
                };
                store.record_quota(&reading("shared", percent, observed))?;
            }
            drop(store);
            legacy.push(Legacy {
                id: session.id,
                child: child.id,
                archived: archived.id,
                workspace: workspace.canonicalize()?,
            });
        }
        let b = legacy.pop().unwrap();
        let a = legacy.pop().unwrap();
        Ok(Self {
            _root: root,
            base,
            a,
            b,
        })
    }

    fn central(&self) -> PathBuf {
        self.base.join("store")
    }

    fn digests(&self) -> [BTreeMap<PathBuf, String>; 2] {
        [
            digest_tree(&self.base.join(A)),
            digest_tree(&self.base.join(B)),
        ]
    }
}

fn statuses(summary: &bubblepaw_core::LegacyImportSummary) -> Vec<LegacyImportStatus> {
    summary.sources.iter().map(|s| s.status.clone()).collect()
}

fn wal_bytes(path: &Path) -> u64 {
    std::fs::metadata(format!("{}-wal", path.display()))
        .map(|m| m.len())
        .unwrap_or(0)
}

#[test]
fn two_legacy_stores_import_once_with_chats_workspaces_history_and_quota() -> Result<()> {
    let lab = Lab::new()?;
    let before = lab.digests();
    let store = Store::open_data_dir(lab.central())?;
    let catalog_transactions = store.storage_stats()?.catalog_transaction_count;
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(
        statuses(&summary),
        vec![LegacyImportStatus::Imported, LegacyImportStatus::Imported]
    );
    assert_eq!(
        summary.sources.iter().map(|s| s.chats).collect::<Vec<_>>(),
        vec![2, 2]
    );
    let data = store.data_dir().unwrap().to_owned();
    let mut chat_wal = 0;
    for entry in std::fs::read_dir(data.join("chats"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "db") {
            chat_wal += wal_bytes(&path);
        }
    }
    println!(
        "legacy import measurements: sources=2 chats=4 import_transactions={} catalog_transactions={} catalog_wal_bytes={} chat_wal_bytes={} staging_left={}",
        summary.transactions,
        store.storage_stats()?.catalog_transaction_count - catalog_transactions,
        wal_bytes(&data.join("catalog.db")),
        chat_wal,
        data.join("legacy-import").exists(),
    );
    // Four staged chat copies, one quota batch, one receipt transaction.
    assert_eq!(summary.transactions, 6);
    assert!(!data.join("legacy-import").exists());

    for legacy in [&lab.a, &lab.b] {
        for session in [&legacy.id, &legacy.child, &legacy.archived] {
            assert_eq!(
                store.session_workspace(session)?.as_deref(),
                Some(legacy.workspace.as_path()),
                "workspace of {session}"
            );
        }
        let messages = store.messages(&legacy.id)?;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content.len(), 6000);
        assert_eq!(store.messages(&legacy.child)?.len(), 1);
        assert_eq!(store.messages(&legacy.archived)?.len(), 1);
    }
    assert_eq!(store.list_sessions()?.len(), 6);
    let mut usage: Vec<_> = store
        .request_usage()?
        .into_iter()
        .map(|u| u.request_id)
        .collect();
    usage.sort();
    assert_eq!(usage, ["request-aaaa", "request-abcd"]);
    let history: Vec<_> = store
        .quota_history()?
        .into_iter()
        .map(|r| (r.account, r.used_percent))
        .collect();
    // Legacy changes merge chronologically across stores, each exactly once.
    assert_eq!(
        history,
        [
            ("only-a".to_string(), 10.0),
            ("only-b".to_string(), 10.0),
            ("only-a".to_string(), 20.0),
            ("only-b".to_string(), 20.0),
            ("shared".to_string(), 50.0),
            ("shared".to_string(), 60.0),
            ("shared".to_string(), 70.0),
        ]
    );
    let latest = store.latest_quota()?;
    assert_eq!(
        latest
            .iter()
            .find(|r| r.account == "shared")
            .unwrap()
            .used_percent,
        70.0
    );
    drop(store);
    assert_eq!(lab.digests(), before, "legacy sources changed");

    // A second start imports nothing and writes nothing.
    let store = Store::open_data_dir(lab.central())?;
    let again = store.import_legacy_stores(|_| panic!("merge after receipt"))?;
    assert_eq!(
        statuses(&again),
        vec![
            LegacyImportStatus::AlreadyImported,
            LegacyImportStatus::AlreadyImported
        ]
    );
    assert_eq!(again.transactions, 0);
    assert_eq!(store.list_sessions()?.len(), 6);
    assert_eq!(store.quota_history()?.len(), 7);
    drop(store);
    assert_eq!(lab.digests(), before);
    Ok(())
}

#[test]
fn interrupted_import_resumes_without_duplicates() -> Result<()> {
    let lab = Lab::new()?;
    let (spill, artifact) = legacy_session_files(&lab.base.join(B), &lab.b.id)?;
    let before = lab.digests();
    let store = Store::open_data_dir(lab.central())?;
    // B's host-side merge fails after its chats and quota rows were merged.
    let summary = store.import_legacy_stores(|snapshot| {
        anyhow::ensure!(!snapshot.source.ends_with(B), "synthetic interruption");
        Ok(())
    })?;
    assert_eq!(summary.sources[0].status, LegacyImportStatus::Imported);
    assert!(matches!(
        &summary.sources[1].status,
        LegacyImportStatus::Skipped(reason) if reason.contains("synthetic interruption")
    ));
    drop(store);
    // B's files were moved before its merge failed; a crash mid-copy would
    // leave only some of them. The unrecorded source copies them again.
    let spilled = lab.central().join("spill").join(&lab.b.id).join(&spill);
    std::fs::remove_file(&spilled)?;

    // A crash after A's merge but before its receipt: drop that receipt and
    // leave a stale staging directory behind.
    let catalog = Connection::open(lab.central().join("catalog.db"))?;
    assert_eq!(
        catalog.execute(
            "DELETE FROM metadata WHERE key=?1",
            [format!("legacy_import:{A}")]
        )?,
        1
    );
    drop(catalog);
    std::fs::create_dir_all(lab.central().join("legacy-import").join(A).join("chats"))?;
    std::fs::write(
        lab.central()
            .join("legacy-import")
            .join(A)
            .join("chats")
            .join("junk.db.part"),
        b"partial",
    )?;

    let store = Store::open_data_dir(lab.central())?;
    let resumed = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(
        statuses(&resumed),
        vec![LegacyImportStatus::Imported, LegacyImportStatus::Imported]
    );
    // Chats are recognized by provenance, quota rows by content.
    assert_eq!(
        resumed
            .sources
            .iter()
            .map(|s| (s.chats, s.quota_rows))
            .collect::<Vec<_>>(),
        vec![(2, 0), (2, 0)]
    );
    assert_eq!(store.list_sessions()?.len(), 6);
    assert_eq!(store.quota_history()?.len(), 7);
    assert_eq!(store.request_usage()?.len(), 2);
    assert_eq!(store.messages(&lab.a.id)?.len(), 2);
    assert_eq!(store.messages(&lab.b.id)?.len(), 3);
    assert_eq!(std::fs::read(&spilled)?, b"full tool output");
    let artifacts = lab.central().join("artifacts").join(&lab.b.id);
    assert!(artifacts.join(format!("{artifact}.bin")).is_file());
    assert!(!lab.central().join("legacy-import").exists());
    let third = store.import_legacy_stores(|_| panic!("merge after receipt"))?;
    assert_eq!(third.transactions, 0);
    drop(store);
    assert_eq!(lab.digests(), before);
    Ok(())
}

#[test]
fn locked_source_is_skipped_and_imported_later() -> Result<()> {
    let lab = Lab::new()?;
    // A running process still owns legacy store B.
    let owner = Store::open_data_dir(lab.base.join(B))?;
    let store = Store::open_data_dir(lab.central())?;
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(
        statuses(&summary),
        vec![LegacyImportStatus::Imported, LegacyImportStatus::Locked]
    );
    assert_eq!(store.list_sessions()?.len(), 3);
    assert!(store.session_workspace(&lab.b.id).is_err());
    drop(store);
    drop(owner);
    // Opening B as its own host may write it; import must not.
    let after_owner = lab.digests()[1].clone();

    let store = Store::open_data_dir(lab.central())?;
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(
        statuses(&summary),
        vec![
            LegacyImportStatus::AlreadyImported,
            LegacyImportStatus::Imported
        ]
    );
    assert_eq!(store.list_sessions()?.len(), 6);
    assert_eq!(
        store.session_workspace(&lab.b.child)?.as_deref(),
        Some(lab.b.workspace.as_path())
    );
    // B's shared reading is older than A's latest change: it is history and
    // lands in time order between A's two changes.
    let history = store.quota_history()?;
    assert_eq!(history.len(), 7);
    let shared: Vec<_> = history
        .iter()
        .filter(|reading| reading.account == "shared")
        .map(|reading| (reading.used_percent, reading.observed_at.clone()))
        .collect();
    assert_eq!(
        shared,
        [
            (50.0, "2026-10-20T01:00:00Z".to_owned()),
            (60.0, "2026-10-20T02:00:00Z".to_owned()),
            (70.0, "2026-10-20T03:00:00Z".to_owned()),
        ]
    );
    // Two chat copies, the side-table rebuild, its swap and the receipt.
    assert_eq!(summary.transactions, 5);
    assert_eq!(summary.sources[1].quota_rows, 3);
    let again = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(again.transactions, 0);
    assert_eq!(store.quota_history()?.len(), 7);
    drop(store);
    assert_eq!(lab.digests()[1], after_owner);
    Ok(())
}

#[test]
fn only_hex_named_directory_stores_are_candidates() -> Result<()> {
    let root = tempfile::tempdir()?;
    let base = root.path().join("BubblePaw-base");
    std::fs::create_dir_all(base.join("not-a-store"))?;
    std::fs::create_dir_all(base.join(&A[..63]))?;
    // A hash directory without a store is reported but never recorded.
    std::fs::create_dir_all(base.join(A))?;
    let store = Store::open_data_dir(base.join("store"))?;
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(summary.sources.len(), 1);
    assert!(matches!(
        &summary.sources[0].status,
        LegacyImportStatus::Skipped(reason) if reason.contains("not a BubblePaw directory store")
    ));
    assert_eq!(summary.transactions, 0);
    assert!(std::fs::read_dir(base.join(A))?.next().is_none());
    Ok(())
}

#[test]
fn quota_merge_commits_bounded_batches() -> Result<()> {
    let root = tempfile::tempdir()?;
    let base = root.path().join("BubblePaw-base");
    let legacy = Store::open_data_dir(base.join(A))?;
    let start = chrono::DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")?;
    for minute in 0..600 {
        let observed = (start + chrono::Duration::minutes(minute))
            .to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        assert!(legacy.record_quota(&reading("busy", (minute % 100) as f64 + 0.5, &observed))?);
    }
    drop(legacy);
    let store = Store::open_data_dir(base.join("store"))?;
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(summary.sources[0].quota_rows, 600);
    // ceil(600 / 256) quota batches plus the receipt; no chats.
    assert_eq!(
        summary.transactions,
        600_usize.div_ceil(bubblepaw_core::LEGACY_IMPORT_BATCH) as u64 + 1
    );
    println!(
        "legacy quota batch measurements: rows=600 transactions={} catalog_wal_bytes={}",
        summary.transactions,
        wal_bytes(&base.join("store").join("catalog.db"))
    );
    assert_eq!(store.quota_history()?.len(), 600);
    Ok(())
}

const C: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[test]
fn older_readings_from_a_late_source_are_history_in_time_order() -> Result<()> {
    let root = tempfile::tempdir()?;
    let base = root.path().join("BubblePaw-base");
    let early = Store::open_data_dir(base.join(A))?;
    early.record_quota(&reading("k", 40.0, "2026-10-20T02:00:00Z"))?;
    drop(early);
    let late = Store::open_data_dir(base.join(B))?;
    let mut first = reading("k", 30.0, "2026-10-20T00:00:00Z");
    first.balance = Some("$7.00".into());
    late.record_quota(&first)?;
    late.record_quota(&reading("k", 40.0, "2026-10-20T01:00:00Z"))?;
    let store = Store::open_data_dir(base.join("store"))?;
    // B is still owned, so only A is imported now.
    store.import_legacy_stores(|_| Ok(()))?;
    drop(late);
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(summary.sources[1].status, LegacyImportStatus::Imported);
    // A's 40 % at 02:00 now repeats B's 40 % at 01:00, so the change-only
    // rule keeps the first observation of that value.
    let history: Vec<_> = store
        .quota_history()?
        .into_iter()
        .map(|reading| (reading.used_percent, reading.observed_at, reading.balance))
        .collect();
    // The source's stored balance column survives the rebuild.
    let balance = || Some("$7.00".to_owned());
    assert_eq!(
        history,
        [
            (30.0, "2026-10-20T00:00:00Z".to_owned(), balance()),
            (40.0, "2026-10-20T01:00:00Z".to_owned(), balance()),
        ]
    );
    assert_eq!(summary.sources[1].quota_rows, 2);
    // Later live readings still follow the rule against the rebuilt history.
    assert!(!store.record_quota(&reading("k", 40.0, "2026-10-20T04:00:00Z"))?);
    assert!(store.record_quota(&reading("k", 45.0, "2026-10-20T05:00:00Z"))?);
    Ok(())
}

#[test]
fn a_copied_legacy_store_is_recorded_without_duplicate_chats() -> Result<()> {
    let lab = Lab::new()?;
    let copy = lab.base.join(C);
    copy_tree(&lab.base.join(A), &copy)?;
    let before = digest_tree(&copy);
    let store = Store::open_data_dir(lab.central())?;
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(statuses(&summary), vec![LegacyImportStatus::Imported; 3]);
    let duplicate = &summary.sources[2];
    let mut duplicates = duplicate.duplicates.clone();
    duplicates.sort();
    let mut expected = vec![lab.a.id.clone(), lab.a.archived.clone()];
    expected.sort();
    assert_eq!(duplicates, expected);
    assert!(duplicate.renamed.is_empty());
    assert_eq!((duplicate.chats, duplicate.quota_rows), (0, 0));
    assert_eq!(store.list_sessions()?.len(), 6);
    assert_eq!(store.quota_history()?.len(), 7);
    assert_eq!(store.request_usage()?.len(), 2);
    assert_eq!(digest_tree(&copy), before);
    drop(store);
    let store = Store::open_data_dir(lab.central())?;
    let again = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(
        statuses(&again),
        vec![LegacyImportStatus::AlreadyImported; 3]
    );
    assert_eq!(again.transactions, 0);
    Ok(())
}

#[test]
fn a_diverged_chat_with_a_taken_id_is_imported_under_new_ids() -> Result<()> {
    let lab = Lab::new()?;
    let copy = lab.base.join(C);
    copy_tree(&lab.base.join(A), &copy)?;
    {
        let diverged = Store::open_data_dir(&copy)?;
        diverged.append_batch(&[uuid_message(&lab.a.id, Role::User, "only in the copy")])?;
    }
    let before = digest_tree(&copy);
    let store = Store::open_data_dir(lab.central())?;
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(statuses(&summary), vec![LegacyImportStatus::Imported; 3]);
    let report = &summary.sources[2];
    assert_eq!(report.duplicates, vec![lab.a.archived.clone()]);
    assert_eq!(report.chats, 1);
    let renamed = report.renamed.get(&lab.a.id).expect("root renamed").clone();
    assert_ne!(renamed, lab.a.id);
    assert_eq!(store.list_sessions()?.len(), 8);
    // The original keeps its history; the diverged copy has its own ids.
    assert_eq!(store.messages(&lab.a.id)?.len(), 2);
    let messages = store.messages(&renamed)?;
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2].content, "only in the copy");
    assert!(messages.iter().all(|message| message.session_id == renamed));
    assert_eq!(
        store.session_workspace(&renamed)?.as_deref(),
        Some(lab.a.workspace.as_path())
    );
    let children: Vec<_> = store
        .list_sessions()?
        .into_iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(renamed.as_str()))
        .collect();
    assert_eq!(children.len(), 1);
    assert_ne!(children[0].id, lab.a.child);
    assert_eq!(store.messages(&children[0].id)?.len(), 1);
    // The receipt keeps the mapping.
    let catalog = Connection::open(lab.central().join("catalog.db"))?;
    let receipt: String = catalog.query_row(
        "SELECT value FROM metadata WHERE key=?1",
        [format!("legacy_import:{C}")],
        |row| row.get(0),
    )?;
    let receipt: serde_json::Value = serde_json::from_str(&receipt)?;
    assert_eq!(receipt["renamed"][&lab.a.id], renamed.as_str());
    drop(catalog);
    assert_eq!(digest_tree(&copy), before);
    drop(store);
    let store = Store::open_data_dir(lab.central())?;
    let again = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(again.transactions, 0);
    assert_eq!(store.list_sessions()?.len(), 8);
    Ok(())
}

#[test]
fn an_unmigrated_legacy_database_is_recorded_once_and_never_written() -> Result<()> {
    let root = tempfile::tempdir()?;
    let base = root.path().join("BubblePaw-base");
    std::fs::create_dir_all(base.join(A))?;
    std::fs::write(base.join(A).join("archive.sqlite"), b"legacy bytes")?;
    let before = digest_tree(&base.join(A));
    let store = Store::open_data_dir(base.join("store"))?;
    assert!(store.legacy_import_pending()?);
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert!(matches!(
        &summary.sources[0].status,
        LegacyImportStatus::Unsupported(reason) if reason.contains("unmigrated")
    ));
    assert_eq!(summary.transactions, 1);
    assert!(!store.legacy_import_pending()?);
    let again = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(statuses(&again), vec![LegacyImportStatus::AlreadyImported]);
    assert_eq!(again.transactions, 0);
    assert_eq!(digest_tree(&base.join(A)), before);
    Ok(())
}

/// A spill file and an artifact for `session` in legacy store `dir`, plus a
/// message that references the spill file by its absolute path.
fn legacy_session_files(dir: &Path, session: &str) -> Result<(String, String)> {
    let spill = dir.join("spill").join(session);
    std::fs::create_dir_all(&spill)?;
    std::fs::write(spill.join(".lock"), b"")?;
    let name = "000000000000000000000000000000000000001-output.txt";
    std::fs::write(spill.join(name), b"full tool output")?;
    let artifacts = dir.join("artifacts").join(session);
    std::fs::create_dir_all(&artifacts)?;
    let artifact = uuid::Uuid::new_v4().to_string();
    std::fs::write(artifacts.join(format!("{artifact}.json")), b"{}")?;
    std::fs::write(artifacts.join(format!("{artifact}.bin")), b"bytes")?;
    let path = dir.canonicalize()?.join("spill").join(session).join(name);
    let receipt = serde_json::json!({"fullOutput": {"path": path}}).to_string();
    let store = Store::open_data_dir(dir)?;
    store.append_batch(&[uuid_message(session, Role::Tool, &receipt)])?;
    Ok((name.to_owned(), artifact))
}

#[test]
fn spill_and_artifacts_move_with_their_chats_so_legacy_dirs_can_go() -> Result<()> {
    let lab = Lab::new()?;
    let (spill, artifact) = legacy_session_files(&lab.base.join(A), &lab.a.id)?;
    let copy = lab.base.join(C);
    copy_tree(&lab.base.join(A), &copy)?;
    {
        let diverged = Store::open_data_dir(&copy)?;
        diverged.append_batch(&[uuid_message(&lab.a.id, Role::User, "only in the copy")])?;
    }
    let before = [digest_tree(&lab.base.join(A)), digest_tree(&copy)];
    let store = Store::open_data_dir(lab.central())?;
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(statuses(&summary), vec![LegacyImportStatus::Imported; 3]);
    let renamed = summary.sources[2].renamed[&lab.a.id].clone();
    assert_eq!(
        [digest_tree(&lab.base.join(A)), digest_tree(&copy)],
        before,
        "a legacy store changed"
    );
    // Without the legacy directories, every chat still has its files and
    // its stored spill paths point into the central store.
    std::fs::remove_dir_all(lab.base.join(A))?;
    std::fs::remove_dir_all(&copy)?;
    drop(store);
    let store = Store::open_data_dir(lab.central())?;
    let central = lab.central().canonicalize()?;
    for session in [&lab.a.id, &renamed] {
        let spill_dir = central.join("spill").join(session);
        assert_eq!(std::fs::read(spill_dir.join(&spill))?, b"full tool output");
        assert!(!spill_dir.join(".lock").exists());
        let artifacts = central.join("artifacts").join(session);
        assert_eq!(
            std::fs::read(artifacts.join(format!("{artifact}.bin")))?,
            b"bytes"
        );
        assert!(artifacts.join(format!("{artifact}.json")).is_file());
        let receipt = store
            .messages(session)?
            .into_iter()
            .find(|message| message.content.contains("fullOutput"))
            .expect("spill receipt");
        let receipt: serde_json::Value = serde_json::from_str(&receipt.content)?;
        assert_eq!(
            receipt["fullOutput"]["path"],
            spill_dir.join(&spill).to_string_lossy().as_ref()
        );
    }
    let again = store.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(again.transactions, 0);
    Ok(())
}

#[test]
fn a_renamed_chat_counts_its_shared_usage_once() -> Result<()> {
    let lab = Lab::new()?;
    let copy = lab.base.join(C);
    copy_tree(&lab.base.join(A), &copy)?;
    {
        let diverged = Store::open_data_dir(&copy)?;
        diverged.append_batch(&[uuid_message(&lab.a.id, Role::User, "only in the copy")])?;
        let mut usage = RequestUsage::new("p", "m");
        usage.session_id.clone_from(&lab.a.id);
        usage.request_id = "request-copy".into();
        usage.usage.input_tokens = 7;
        diverged.queue_request_usage(usage)?;
        diverged.flush_request_usage(&lab.a.id)?;
    }
    // The source totals, with a request present in both A and the copy
    // counted once.
    let mut expected = BTreeMap::new();
    for dir in [lab.base.join(A), lab.base.join(B), copy.clone()] {
        for row in Store::open_data_dir(&dir)?.request_usage()? {
            expected.insert(row.request_id, row.usage.input_tokens);
        }
    }
    assert_eq!(expected.len(), 3, "{expected:?}");
    let store = Store::open_data_dir(lab.central())?;
    let summary = store.import_legacy_stores(|_| Ok(()))?;
    let renamed = summary.sources[2].renamed[&lab.a.id].clone();
    let rows = store.request_usage()?;
    let mut imported = BTreeMap::new();
    for row in &rows {
        assert!(
            imported
                .insert(row.request_id.clone(), row.usage.input_tokens)
                .is_none(),
            "request {} counted twice",
            row.request_id
        );
    }
    assert_eq!(imported, expected);
    let own = rows
        .iter()
        .find(|row| row.request_id == "request-copy")
        .unwrap();
    assert_eq!(own.session_id, renamed);
    Ok(())
}
