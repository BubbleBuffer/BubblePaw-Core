use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn checkpoint_id(name: &str) -> String {
    format!("{CHECKPOINT_PREFIX}{name}")
}

fn counting_hook(counter: Arc<AtomicUsize>) -> ChatOpenHook {
    Arc::new(move |_chat: &Store| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

#[test]
fn checkpoint_prune_never_scans_reference_text() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("scan")?.id;
    fn record(id: &str, patch_index: u64, boundary_seq: i64) -> crate::CheckpointRecord<'_> {
        crate::CheckpointRecord {
            id,
            patch_index,
            boundary_seq,
            metadata: "{}",
        }
    }
    store.write_checkpoint(&session, record(&checkpoint_id("a"), 1, 0), b"one", &[])?;
    let before = Store::reference_scans();
    let prunes = [(session.clone(), 3)];
    store.write_checkpoint(&session, record(&checkpoint_id("b"), 2, 5), b"two", &prunes)?;
    assert_eq!(Store::reference_scans(), before);
    Ok(())
}

#[test]
fn chat_open_hook_runs_once_per_chat_on_first_open() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let opened = Arc::new(AtomicUsize::new(0));
    store.on_chat_open("test.counter", counting_hook(opened.clone()))?;
    let session = store.create_session("hook")?.id;
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    store.store_attachment(&session, "x", "{}", b"x")?;
    store.chat_sessions(&session)?;
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn hook_installed_after_open_runs_on_chats_already_open() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("cached")?.id;
    // The chat is cached before the hook exists, as the constructor leaves it.
    let opened = Arc::new(AtomicUsize::new(0));
    store.on_chat_open("test.cached", counting_hook(opened.clone()))?;
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    store.store_attachment(&session, "x", "{}", b"x")?;
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn chat_open_hook_key_registers_once() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let opened = Arc::new(AtomicUsize::new(0));
    store.on_chat_open("test.dedupe", counting_hook(opened.clone()))?;
    store.create_session("dedupe")?;
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    // A second registration under the same key is ignored, so the cached chat
    // does not run it and no open runs it twice.
    store.on_chat_open("test.dedupe", counting_hook(opened.clone()))?;
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    store.create_session("dedupe-again")?;
    assert_eq!(opened.load(Ordering::SeqCst), 2);
    Ok(())
}

#[test]
fn failing_hook_on_one_cached_chat_does_not_stop_the_others() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    store.create_session("first-cached")?;
    store.create_session("second-cached")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let hook: ChatOpenHook = Arc::new(move |_chat: &Store| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            anyhow::bail!("first chat is broken");
        }
        Ok(())
    });
    let failures = store.on_chat_open("test.partial", hook)?;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "both chats ran the hook");
    assert_eq!(failures.len(), 1);
    assert!(format!("{:#}", failures[0].error).contains("first chat is broken"));
    assert!(
        failures[0]
            .chat
            .starts_with(directory.path().canonicalize()?)
    );
    Ok(())
}
