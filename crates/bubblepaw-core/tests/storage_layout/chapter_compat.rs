//! Chapter metadata compatibility keeps the existing test coverage unchanged.
use super::*;

#[test]
fn failed_staged_migration_leaves_source_unchanged_and_a_repaired_fixture_can_retry() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("legacy.sqlite");
    let target = directory.path().join("data");
    let (sessions, original, _) = legacy_fixture(&source, false)?;
    // The unsupported version is discovered on the staged copy, after the
    // source snapshot was taken. No incomplete target may be installed.
    Connection::open(&source)?.execute_batch("PRAGMA user_version=999")?;
    let invalid_bytes = std::fs::read(&source)?;
    assert!(Store::migrate_legacy(&source, &target).is_err());
    assert_eq!(std::fs::read(&source)?, invalid_bytes);
    assert!(!target.exists());
    Connection::open(&source)?.execute_batch("PRAGMA user_version=6")?;
    let repaired_bytes = std::fs::read(&source)?;
    Store::migrate_legacy(&source, &target)?;
    let store = Store::open_data_dir(&target)?;
    assert_eq!(store.list_sessions()?.len(), 2);
    for (session, expected) in sessions.iter().zip(original) {
        assert_eq!(store.messages(&session.id)?, vec![expected]);
    }
    assert_eq!(std::fs::read(&source)?, repaired_bytes);
    Ok(())
}
