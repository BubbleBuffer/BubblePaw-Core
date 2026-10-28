//! Bounded session-owned text output, separate from authoritative SQLite rows.
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{Store, ToolContext, ToolRegistry, ToolResult};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub const CHAT_LIMIT_BYTES: u64 = 256 * 1024 * 1024;
pub const RESULT_LIMIT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct SpillStore {
    data_dir: PathBuf,
    chat_limit: u64,
}
impl SpillStore {
    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    pub fn new(data_dir: impl AsRef<Path>) -> Result<Self> {
        Self::with_chat_limit(data_dir, CHAT_LIMIT_BYTES)
    }
    /// A smaller retention limit is useful for deterministic host fixtures.
    pub fn with_chat_limit(data_dir: impl AsRef<Path>, chat_limit: u64) -> Result<Self> {
        ensure!(chat_limit > 0, "spill limit must be positive");
        fs::create_dir_all(data_dir.as_ref())?;
        Ok(Self {
            data_dir: data_dir.as_ref().canonicalize()?,
            chat_limit,
        })
    }
    fn directory(&self, session: &str, create: bool) -> Result<PathBuf> {
        valid_session(session)?;
        let mut directory = self.data_dir.clone();
        for part in ["spill", session] {
            directory.push(part);
            if create && !directory.exists() {
                match fs::create_dir(&directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let metadata = fs::symlink_metadata(&directory)?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "spill directory must be a regular directory"
            );
            ensure!(
                directory.canonicalize()?.starts_with(&self.data_dir),
                "spill directory escapes data directory"
            );
        }
        Ok(directory)
    }
    pub fn save(&self, session: &str, content: &str) -> Result<Value> {
        ensure!(
            content.len() as u64 <= self.chat_limit,
            "output exceeds the per-chat spill limit ({} bytes); full output was not saved",
            self.chat_limit
        );
        let directory = self.directory(session, true)?;
        // Separate instances/hosts use the same lock; the lock itself is not output.
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(".lock"))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let metadata = entry.file_type()?;
            if metadata.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "txt")
            {
                let bytes = entry.metadata()?.len();
                total = total.saturating_add(bytes);
                files.push((
                    entry.metadata()?.modified()?,
                    entry.file_name(),
                    bytes,
                    entry.path(),
                ));
            }
        }
        files.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
        for (_, _, bytes, path) in files {
            if total.saturating_add(content.len() as u64) <= self.chat_limit {
                break;
            }
            fs::remove_file(path)?;
            total -= bytes;
        }
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = directory.join(format!("{stamp:039}-{}.txt", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        if let Err(error) = file
            .write_all(content.as_bytes())
            .and_then(|()| file.sync_data())
        {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error.into());
        }
        Ok(json!({"path":path,"bytes":content.len(),"format":"text"}))
    }
    /// Only canonical descendants of this session's directory are readable.
    pub fn readable(&self, session: &str, path: &Path) -> Result<PathBuf> {
        valid_session(session)?;
        ensure!(path.is_absolute(), "spill reads require an absolute path");
        let resolved = path
            .canonicalize()
            .context("spill output is missing or was evicted")?;
        ensure!(
            resolved.starts_with(self.data_dir.join("spill").join(session)),
            "path is outside this chat's spill directory"
        );
        self.directory(session, false)?;
        ensure!(resolved.is_file(), "spill path must be a regular file");
        Ok(resolved)
    }
    pub fn delete_session(&self, session: &str) -> Result<()> {
        valid_session(session)?;
        let candidate = self.data_dir.join("spill").join(session);
        if !candidate.try_exists()? {
            return Ok(());
        }
        let directory = self.directory(session, false)?;
        fs::remove_dir_all(directory).context("chat deleted, but spill cleanup failed")
    }
    /// Register at host startup, including hosts that only delete a stored chat.
    pub fn attach(&self, store: &Store) {
        crate::scratchpads::ScratchpadStore::register_cleanup_at(store, &self.data_dir);
        let spills = self.clone();
        store.set_session_cleanup(
            "runner.spill",
            Arc::new(move |session| spills.delete_session(session)),
        );
    }
    pub fn register(&self, registry: &mut ToolRegistry) {
        let spills = self.clone();
        registry.set_result_limiter(Arc::new(move |store, context, result, limit| {
            spills.attach(store);
            spills.limit(context, result, limit)
        }));
    }
    fn limit(
        &self,
        context: &ToolContext,
        mut result: ToolResult,
        limit: usize,
    ) -> Result<ToolResult> {
        if result.content.len() <= limit {
            return Ok(result);
        }
        let mut receipt = json!({"truncated":true,"previewFormat":"text","preview":""});
        match self.save(&context.session_id, &result.content) {
            Ok(reference) => receipt["fullOutput"] = reference,
            Err(error) => {
                receipt["fullOutputError"] = json!(format!("Could not save full output: {error}"))
            }
        }
        // Preserve useful process handles and status even when a native result spills.
        if let Ok(value) = serde_json::from_str::<Value>(&result.content) {
            for key in ["handle", "status", "cursor", "exitCode"] {
                if let Some(value) = value.get(key)
                    && value.to_string().len() <= 1024
                {
                    receipt[key] = value.clone();
                }
            }
        }
        let mut budget = limit.saturating_sub(receipt.to_string().len() + 128) / 6;
        loop {
            receipt["preview"] = json!(preview(&result.content, budget));
            if receipt.to_string().len() <= limit || budget == 0 {
                break;
            }
            budget /= 2;
        }
        result.content = receipt.to_string();
        Ok(result)
    }
}
fn valid_session(session: &str) -> Result<()> {
    ensure!(
        !session.is_empty()
            && session.len() <= 200
            && session
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || character == '-'
                    || character == '_'),
        "invalid spill session id"
    );
    Ok(())
}
pub(crate) fn preview(content: &str, bytes: usize) -> String {
    let mut head = content.len().min(bytes.div_ceil(2));
    while !content.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = content.len().saturating_sub(bytes / 2).max(head);
    while !content.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[... output preview truncated ...]\n{}",
        &content[..head],
        &content[tail..]
    )
}
