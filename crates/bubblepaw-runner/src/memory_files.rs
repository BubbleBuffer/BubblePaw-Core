//! Host-owned repository instructions. The immutable session attachment is the
//! frozen system prefix; edits travel once through ordinary durable host notices.
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{MessageOrigin, Store};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::live_config::{FileStamp, file_stamps};

pub const MAX_MEMORY_BYTES: usize = 32 * 1024;
const ATTACHMENT: &str = "bubblepaw-memory-files-v1";
const NOTICE: &str = "Memory files changed (the session system prefix stays frozen):\n";
const PREFIX: &str = "Repository memory files (frozen at session start):\n";
const TRUNCATED: &str = "\n[Memory files truncated at 32 KiB; remaining content omitted.]\n";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileContent {
    path: PathBuf,
    /// None means that a previously loaded file was removed.
    content: Option<String>,
    digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duplicate_of: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Snapshot {
    files: Vec<FileContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    truncation: Option<String>,
}
impl Snapshot {
    fn prompt(&self) -> String {
        let mut prompt = String::from(PREFIX);
        let mut truncated = false;
        for file in &self.files {
            if let Some(content) = &file.content
                && file.duplicate_of.is_none()
            {
                prompt.push_str(&format!("\n--- {} ---\n{content}\n", file.path.display()));
                truncated |= file.truncated;
            }
        }
        if prompt == PREFIX {
            return String::new();
        }
        if truncated || prompt.len() > MAX_MEMORY_BYTES {
            truncate_utf8(&mut prompt, MAX_MEMORY_BYTES - TRUNCATED.len());
            prompt.push_str(TRUNCATED);
        }
        prompt
    }

    fn apply(&mut self, changes: Snapshot) {
        for file in changes.files {
            if let Some(previous) = self.files.iter_mut().find(|old| old.path == file.path) {
                *previous = file;
            } else {
                self.files.push(file);
            }
        }
    }
}

struct SessionMemory {
    frozen: String,
    observed: Snapshot,
    stamps: Vec<FileStamp>,
    pending: Vec<String>,
}

pub struct MemoryFiles {
    store: Store,
    workspace: PathBuf,
    user_directory: Option<PathBuf>,
    enabled: bool,
    sessions: Mutex<HashMap<String, SessionMemory>>,
}
impl MemoryFiles {
    pub fn new(
        store: Store,
        workspace: PathBuf,
        user_directory: Option<PathBuf>,
        enabled: bool,
    ) -> Self {
        Self {
            store,
            workspace,
            user_directory,
            enabled,
            sessions: Mutex::default(),
        }
    }

    /// Called alongside the host's turn-start config stat. An unchanged check
    /// performs no reads of file contents and no application write transactions.
    pub fn check(&self, session: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut sessions = self.sessions.lock().unwrap();
        let workspace = crate::worktrees::workspace(&self.store, session, &self.workspace)?;
        let paths = discover(&workspace, self.user_directory.as_deref())?;
        let stamps = file_stamps(paths)?;
        if !sessions.contains_key(session) {
            let current = load(&stamps)?;
            let (frozen, mut observed) = match self.store.attachment(session, ATTACHMENT)? {
                Some((_, bytes)) => {
                    let frozen: Snapshot = serde_json::from_slice(&bytes)
                        .context("Invalid saved memory-file snapshot")?;
                    let mut observed = frozen.clone();
                    // Notices are canonical complete messages. Replaying their
                    // tiny diffs here prevents redelivery after a host restart.
                    self.store
                        .for_each_message_range(session, 0, i64::MAX, |_, message| {
                            if message.origin == Some(MessageOrigin::Generated) {
                                for changes in message.content.split(NOTICE).skip(1) {
                                    if let Some(Ok(changes)) =
                                        serde_json::Deserializer::from_str(changes)
                                            .into_iter::<Snapshot>()
                                            .next()
                                    {
                                        observed.apply(changes);
                                    }
                                }
                            }
                            Ok(())
                        })?;
                    (frozen.prompt(), observed)
                }
                None => {
                    self.store.store_attachment(
                        session,
                        ATTACHMENT,
                        "{\"kind\":\"memory-files\",\"version\":1}",
                        &serde_json::to_vec(&current)?,
                    )?;
                    (current.prompt(), current.clone())
                }
            };
            // A resumed session still uses its original prefix, but disk edits
            // made while the host was stopped must become one new notice.
            let pending = changes(&observed, &current, &stamps)?;
            observed = current;
            sessions.insert(
                session.into(),
                SessionMemory {
                    frozen,
                    observed,
                    stamps,
                    pending,
                },
            );
            return Ok(());
        }
        let state = sessions
            .get_mut(session)
            .expect("initialized session memory");
        if state.stamps != stamps {
            let current = load(&stamps)?;
            state
                .pending
                .extend(changes(&state.observed, &current, &stamps)?);
            state.observed = current;
            state.stamps = stamps;
        }
        Ok(())
    }

    pub fn system_prompt(&self, session: &str, base: &str) -> Result<String> {
        if self.enabled && !self.sessions.lock().unwrap().contains_key(session) {
            self.check(session)?;
        }
        let sessions = self.sessions.lock().unwrap();
        Ok(
            match sessions
                .get(session)
                .filter(|state| !state.frozen.is_empty())
            {
                Some(state) => format!("{base}\n\n{}", state.frozen),
                None => base.to_owned(),
            },
        )
    }

    pub fn take_changes(&self, session: &str) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap()
            .get_mut(session)
            .map(|state| std::mem::take(&mut state.pending))
            .unwrap_or_default()
    }
}

/// User config AGENTS first; then both names at each level from the nearest git
/// root through the workspace. A worktree's .git file is also a root marker.
fn discover(workspace: &Path, user_directory: Option<&Path>) -> Result<Vec<PathBuf>> {
    let workspace = workspace
        .canonicalize()
        .context("Memory-file workspace must exist")?;
    ensure!(
        workspace.is_dir(),
        "Memory-file workspace must be a directory"
    );
    let root = workspace
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(&workspace);
    let mut directories = Vec::new();
    for directory in workspace.ancestors() {
        directories.push(directory);
        if directory == root {
            break;
        }
    }
    let mut paths = Vec::new();
    if let Some(user) = user_directory {
        paths.push(user.join("AGENTS.md"));
    }
    for directory in directories.into_iter().rev() {
        paths.push(directory.join("AGENTS.md"));
        paths.push(directory.join("CLAUDE.md"));
    }
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    Ok(paths)
}

fn load(stamps: &[FileStamp]) -> Result<Snapshot> {
    let mut snapshot = Snapshot::default();
    let mut available = MAX_MEMORY_BYTES.saturating_sub(PREFIX.len() + TRUNCATED.len());
    let mut seen = HashMap::new();
    for stamp in stamps {
        let path = stamp.path.clone();
        let Some(size) = stamp.size else {
            continue;
        };
        // Keep only bounded text; stream the digest so an edit beyond the cap
        // still produces a notice and an identical rewrite produces none.
        let mut bytes = Vec::new();
        let mut reader = std::fs::File::open(&path)
            .with_context(|| format!("Cannot read memory file {}", path.display()))?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 8192];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            let keep = count.min((MAX_MEMORY_BYTES + 1).saturating_sub(bytes.len()));
            bytes.extend_from_slice(&buffer[..keep]);
        }
        let digest = format!("{:x}", hash.finalize());
        let duplicate_of = seen.get(&digest).cloned();
        seen.entry(digest.clone()).or_insert_with(|| path.clone());
        let mut content = String::from_utf8_lossy(&bytes).into_owned();
        let mut truncated = size > bytes.len() as u64;
        if duplicate_of.is_some() {
            content.clear();
        } else {
            available = available.saturating_sub(format!("\n--- {} ---\n\n", path.display()).len());
            truncated |= content.len() > available;
            truncate_utf8(&mut content, available);
            available = available.saturating_sub(content.len());
        }
        snapshot.files.push(FileContent {
            path,
            content: Some(content),
            digest: Some(digest),
            duplicate_of,
            truncated,
        });
    }
    Ok(snapshot)
}

fn changes(before: &Snapshot, after: &Snapshot, stamps: &[FileStamp]) -> Result<Vec<String>> {
    let changed_paths: HashSet<_> = after
        .files
        .iter()
        .filter(|file| {
            !before
                .files
                .iter()
                .any(|old| old.path == file.path && old.digest == file.digest)
        })
        .map(|file| &file.path)
        .collect();
    // A changed file gets a fresh note budget even if an unchanged earlier file
    // consumed the entire frozen prefix budget.
    let changed_stamps = stamps
        .iter()
        .filter(|stamp| changed_paths.contains(&stamp.path))
        .cloned()
        .collect::<Vec<_>>();
    let mut delta = load(&changed_stamps)?;
    for file in &before.files {
        if file.content.is_some() && !after.files.iter().any(|new| new.path == file.path) {
            delta.files.push(FileContent {
                path: file.path.clone(),
                content: None,
                digest: None,
                duplicate_of: None,
                truncated: false,
            });
        }
    }
    if delta.files.is_empty() {
        return Ok(Vec::new());
    }
    // JSON makes changed paths and content unambiguous when a file itself holds
    // notice-like text. Content is trimmed, never the parseable notice record.
    loop {
        let encoded = serde_json::to_string(&delta)?;
        if NOTICE.len() + encoded.len() <= MAX_MEMORY_BYTES {
            return Ok(vec![format!("{NOTICE}{encoded}")]);
        }
        let largest = delta
            .files
            .iter_mut()
            .filter(|file| file.content.as_ref().is_some_and(|text| !text.is_empty()))
            .max_by_key(|file| file.content.as_ref().map_or(0, String::len));
        let Some(file) = largest else {
            anyhow::bail!("Memory-file change paths exceed the 32 KiB notice limit");
        };
        let content = file.content.as_mut().expect("selected nonempty content");
        truncate_utf8(content, content.len() / 2);
        file.truncated = true;
        delta.truncation =
            Some("Memory files truncated at 32 KiB; remaining changed content omitted.".into());
    }
}

fn truncate_utf8(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}
