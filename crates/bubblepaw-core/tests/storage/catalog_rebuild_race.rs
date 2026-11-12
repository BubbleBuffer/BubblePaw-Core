//! A catalog rebuild runs whenever a lookup misses, and it replaces the catalog
//! from a scan of the chat files. A session committed while that scan runs must
//! survive it: an unknown-id lookup on one thread must not unregister a session
//! another thread has just created.
use anyhow::Result;
use bubblepaw_core::Store;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn rebuild_keeps_sessions_created_while_it_scans() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    // Chats to scan, so each rebuild takes long enough to overlap creation.
    for i in 0..30 {
        store.create_session(format!("existing {i}"))?;
    }
    let done = AtomicBool::new(false);
    let created = std::thread::scope(|scope| -> Result<Vec<String>> {
        let rebuilder = scope.spawn(|| -> Result<()> {
            while !done.load(Ordering::Relaxed) {
                store.catalog_rebuild()?;
            }
            Ok(())
        });
        let mut created = Vec::new();
        let outcome = (|| -> Result<()> {
            for i in 0..60 {
                let session = store.create_session(format!("racing {i}"))?;
                created.push(session.id.clone());
                store.session_root_id(&session.id).map_err(|error| {
                    anyhow::anyhow!("session {i} vanished after creation: {error}")
                })?;
            }
            Ok(())
        })();
        done.store(true, Ordering::Relaxed);
        rebuilder.join().expect("rebuild thread panicked")?;
        outcome?;
        Ok(created)
    })?;
    for id in &created {
        store
            .session_root_id(id)
            .map_err(|error| anyhow::anyhow!("session {id} lost by a rebuild: {error}"))?;
    }
    Ok(())
}
