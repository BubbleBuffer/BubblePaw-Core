//! A previous default store (`BubblePaw-base/store`, from before the central
//! store moved to `BubblePaw-Core`) is a legacy source: imported once, read
//! only, and the per-workspace stores it already absorbed are not copied again.
use anyhow::Result;
use bubblepaw_core::{LegacyImportStatus, LegacyImportSummary, Store};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn statuses(summary: &LegacyImportSummary) -> Vec<LegacyImportStatus> {
    summary.sources.iter().map(|s| s.status.clone()).collect()
}

/// Every file under `root` with its bytes, to prove a store was only read.
fn tree(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.strip_prefix(root)?.to_owned(), std::fs::read(&path)?);
            }
        }
    }
    Ok(files)
}

fn chat_files(store: &Store) -> Result<usize> {
    let chats = store.data_dir().unwrap().join("chats");
    Ok(std::fs::read_dir(chats)?
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|entry| entry.path().extension().is_some_and(|e| e == "db"))
        })
        .count())
}

#[test]
fn previous_default_store_imports_once_and_absorbs_its_own_legacy_stores() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let old_base = root.path().join("BubblePaw-base");
    // The old binary imported a per-workspace store into the previous default store.
    let sibling = Store::open_data_dir(old_base.join(A))?;
    sibling.create_session_with_workspace(String::from("per-workspace chat"), &workspace)?;
    drop(sibling);
    let previous_dir = old_base.join("store");
    let previous = Store::open_data_dir(&previous_dir)?;
    previous.create_session_with_workspace(String::from("default chat"), &workspace)?;
    let imported = previous.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(statuses(&imported), vec![LegacyImportStatus::Imported]);
    drop(previous);
    let before = tree(&previous_dir)?;

    let central = Store::open_data_dir(root.path().join("BubblePaw-Core").join("store"))?;
    let previous_list = vec![previous_dir.clone()];
    assert!(central.legacy_import_pending_with(&previous_list)?);
    let first = central.import_legacy_stores_with(&previous_list, |_| Ok(()))?;
    assert_eq!(
        statuses(&first),
        vec![
            LegacyImportStatus::Imported,
            LegacyImportStatus::AlreadyImported
        ]
    );
    assert_eq!(first.sources[0].chats, 2, "default chat and absorbed chat");
    assert_eq!(first.sources[1].chats, 0, "absorbed store is not copied");
    assert_eq!(chat_files(&central)?, 2);
    assert_eq!(
        tree(&previous_dir)?,
        before,
        "the previous store is only read"
    );

    assert!(!central.legacy_import_pending_with(&previous_list)?);
    let again =
        central.import_legacy_stores_with(&previous_list, |_| panic!("merge after receipt"))?;
    assert_eq!(
        statuses(&again),
        vec![
            LegacyImportStatus::AlreadyImported,
            LegacyImportStatus::AlreadyImported
        ]
    );
    assert_eq!(chat_files(&central)?, 2);

    // A store opened at an explicit location never sees the previous default store.
    let explicit = Store::open_data_dir(root.path().join("explicit").join("store"))?;
    assert!(!explicit.legacy_import_pending()?);
    assert!(
        explicit
            .import_legacy_stores(|_| Ok(()))?
            .sources
            .is_empty()
    );
    Ok(())
}
