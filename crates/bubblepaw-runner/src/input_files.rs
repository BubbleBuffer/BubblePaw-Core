//! Host-owned attachment paths with optional canonical SQLite blob backing.
use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::Store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct InputLimits {
    pub prompt_chars: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_files: usize,
    pub inline_tokens: usize,
}
impl Default for InputLimits {
    fn default() -> Self {
        Self {
            prompt_chars: 32_000,
            max_file_bytes: 16 * 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
            max_files: 16,
            inline_tokens: 10_000,
        }
    }
}
impl InputLimits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.prompt_chars > 0
                && self.max_file_bytes > 0
                && self.max_total_bytes > 0
                && self.max_files > 0
                && self.inline_tokens > 0,
            "Input limits must be positive"
        );
        ensure!(
            self.max_file_bytes < usize::MAX as u64,
            "File limit exceeds host capacity"
        );
        Ok(())
    }
    pub fn validate_prompt(&self, text: &str) -> Result<()> {
        self.validate_message(text, false)
    }
    fn validate_message(&self, text: &str, attachments: bool) -> Result<()> {
        ensure!(
            text.chars().count() <= self.prompt_chars,
            "Prompt exceeds the {} Unicode character limit; attach larger text as a file",
            self.prompt_chars
        );
        ensure!(
            attachments || !text.trim().is_empty(),
            "Text or at least one attachment is required"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputFileRef {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub path: String,
    pub url: String,
    pub text: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<bool>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sidecar {
    session: String,
    file: InputFileRef,
    sha256: String,
}

#[derive(Clone)]
pub struct InputFiles {
    directory: PathBuf,
    limits: InputLimits,
    store: Option<Store>,
}

impl std::fmt::Debug for InputFiles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputFiles")
            .field("directory", &self.directory)
            .field("limits", &self.limits)
            .field("record_backed", &self.store.is_some())
            .finish()
    }
}

impl InputFiles {
    pub fn new(workspace: impl AsRef<Path>, limits: InputLimits) -> Result<Self> {
        limits.validate()?;
        let workspace = workspace
            .as_ref()
            .canonicalize()
            .context("Workspace is unavailable")?;
        ensure!(workspace.is_dir(), "Workspace must be a directory");
        let run = directory_child(&workspace, ".run", true)?;
        let directory = directory_child(&run, "attachments", true)?;
        Ok(Self {
            directory,
            limits,
            store: None,
        })
    }
    /// Retain large originals in the record; existing paths remain host caches.
    pub fn with_store(mut self, store: Store) -> Self {
        self.store = Some(store);
        self
    }
    pub fn limits(&self) -> InputLimits {
        self.limits
    }

    pub fn upload(&self, session: &str, name: &str, bytes: &[u8]) -> Result<InputFileRef> {
        valid_id(session)?;
        ensure!(
            bytes.len() as u64 <= self.limits.max_file_bytes,
            "Attachment exceeds the {} byte file limit",
            self.limits.max_file_bytes
        );
        let session_dir = self.session_dir(session, true)?;
        let id = Uuid::new_v4().to_string();
        let directory = directory_child(&session_dir, &id, true)?;
        let name = safe_name(name);
        let file = reference(session, &id, &name, bytes.len() as u64, is_text(bytes));
        let metadata = Sidecar {
            session: session.into(),
            file: file.clone(),
            sha256: hash(bytes),
        };
        let result = (|| -> Result<()> {
            save(&directory.join(&name), bytes)?;
            save(
                &session_dir.join(format!("{id}.json")),
                &serde_json::to_vec(&metadata)?,
            )?;
            Ok(())
        })();
        if result.is_err() {
            // An incomplete UUID directory is unreferenced and never admitted.
            let _ = fs::remove_file(session_dir.join(format!("{id}.json")));
        }
        result?;
        if bytes.len() > 4096
            && let Some(store) = &self.store
        {
            store.store_attachment(session, &id, &serde_json::to_string(&metadata)?, bytes)?;
        }
        Ok(file)
    }

    pub fn read(&self, session: &str, id: &str) -> Result<(InputFileRef, Vec<u8>)> {
        valid_id(id)?;
        valid_id(session)?;
        let stored = self
            .store
            .as_ref()
            .map(|store| store.attachment(session, id))
            .transpose()?
            .flatten();
        let (sidecar, stored_bytes): (Sidecar, Option<Vec<u8>>) = match stored {
            Some((metadata, bytes)) => (serde_json::from_str(&metadata)?, Some(bytes)),
            None => {
                let session_dir = self.session_dir(session, false)?;
                (
                    serde_json::from_slice(&read_regular(
                        &session_dir.join(format!("{id}.json")),
                        8192,
                    )?)?,
                    None,
                )
            }
        };
        let file = sidecar.file;
        ensure!(
            sidecar.session == session
                && file.id == id
                && file.name == safe_name(&file.name)
                && file.inline.is_none()
                && file.omitted.is_none(),
            "Attachment ownership or metadata is invalid"
        );
        ensure!(
            file.size <= self.limits.max_file_bytes,
            "Attachment exceeds the configured file limit"
        );
        ensure!(
            file == reference(session, id, &file.name, file.size, file.text),
            "Attachment reference is invalid"
        );
        let from_record = stored_bytes.is_some();
        let bytes = match stored_bytes {
            Some(bytes) => bytes,
            None => {
                let session_dir = self.session_dir(session, false)?;
                let directory = directory_child(&session_dir, id, false)?;
                read_regular(&directory.join(&file.name), self.limits.max_file_bytes)?
            }
        };
        ensure!(
            bytes.len() as u64 == file.size
                && hash(&bytes) == sidecar.sha256
                && is_text(&bytes) == file.text,
            "Attachment bytes do not match immutable metadata"
        );
        if from_record {
            // Messages expose paths for filesystem tools as well as host reads.
            let session_dir = self.session_dir(session, true)?;
            let directory = directory_child(&session_dir, id, true)?;
            let path = directory.join(&file.name);
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => save(&path, &bytes)?,
                Err(error) => return Err(error.into()),
                Ok(_) => ensure!(
                    read_regular(&path, self.limits.max_file_bytes)? == bytes,
                    "Attachment cache does not match immutable metadata"
                ),
            }
        }
        Ok((file, bytes))
    }

    /// Append only host-validated references. Inline content is all-or-nothing per file.
    pub fn prepare(&self, session: &str, text: &str, ids: &[String]) -> Result<String> {
        self.limits.validate_message(text, !ids.is_empty())?;
        ensure!(
            ids.len() <= self.limits.max_files,
            "At most {} attachments are allowed per message (metadata guard)",
            self.limits.max_files
        );
        if ids.is_empty() {
            return Ok(text.into());
        }
        let mut seen = HashSet::new();
        let mut total = 0u64;
        let mut files = Vec::new();
        let mut candidates = Vec::new();
        for id in ids {
            ensure!(seen.insert(id), "Duplicate attachment IDs are not allowed");
            let (mut file, bytes) = self.read(session, id)?;
            total = total
                .checked_add(file.size)
                .context("Attachment total overflow")?;
            ensure!(
                total <= self.limits.max_total_bytes,
                "Attachments exceed the {} byte combined message limit",
                self.limits.max_total_bytes
            );
            let candidate = if file.text {
                file.omitted = Some(true);
                // Bound tokenizer work for large files; their full bytes remain accessible by path.
                if bytes.len() <= self.limits.inline_tokens.saturating_mul(8) {
                    Some(String::from_utf8(bytes)?)
                } else {
                    None
                }
            } else {
                None
            };
            candidates.push(candidate);
            files.push(file);
        }
        ensure!(
            envelope_tokens(&files)? <= self.limits.inline_tokens,
            "Attachment reference metadata exceeds the envelope token limit"
        );
        for (index, candidate) in candidates.into_iter().enumerate() {
            if let Some(candidate) = candidate {
                files[index].inline = Some(candidate);
                files[index].omitted = None;
                if envelope_tokens(&files)? > self.limits.inline_tokens {
                    files[index].inline = None;
                    files[index].omitted = Some(true);
                }
            }
        }
        let prefix = if text.is_empty() {
            "Attached files."
        } else {
            text
        };
        Ok(format!("{prefix}{}", envelope(&files)?))
    }

    fn session_dir(&self, session: &str, create: bool) -> Result<PathBuf> {
        valid_id(session)?;
        ensure_directory(&self.directory)?;
        directory_child(&self.directory, session, create)
    }
}

fn reference(session: &str, id: &str, name: &str, size: u64, text: bool) -> InputFileRef {
    InputFileRef {
        id: id.into(),
        name: name.into(),
        size,
        path: format!(".run/attachments/{session}/{id}/{name}"),
        url: format!("/api/sessions/{session}/attachments/{id}"),
        text,
        inline: None,
        omitted: None,
    }
}
fn envelope(files: &[InputFileRef]) -> Result<String> {
    Ok(format!(
        "\n\n<BubblePawAttachments>\n{}\n</BubblePawAttachments>",
        serde_json::to_string(&serde_json::json!({"files":files}))?
    ))
}
fn envelope_tokens(files: &[InputFileRef]) -> Result<usize> {
    Ok(tiktoken_rs::o200k_base_singleton()
        .encode_ordinary(&envelope(files)?)
        .len())
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn is_text(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| {
        !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    })
}
fn valid_id(value: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(value)
            .context("Invalid attachment or session ID")?
            .to_string()
            == value,
        "IDs must be canonical UUIDs"
    );
    Ok(())
}
fn safe_name(name: &str) -> String {
    let mut name: String = name
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '/' | '\\' | ':' | '"' | ';' | '<' | '>' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                c
            }
        })
        .take(120)
        .collect();
    name = name.trim_matches([' ', '.']).to_owned();
    while name.len() > 180 {
        name.pop();
    }
    name = name.trim_matches([' ', '.']).to_owned();
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if name.is_empty() {
        name = "attachment".into();
    } else if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
    {
        name.insert(0, '_');
    }
    name
}
fn ensure_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink() && path.canonicalize()? == path,
        "Attachment storage cannot contain redirected directories"
    );
    Ok(())
}
fn directory_child(parent: &Path, name: &str, create: bool) -> Result<PathBuf> {
    ensure_directory(parent)?;
    let child = parent.join(name);
    if create {
        match fs::create_dir(&child) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    ensure_directory(&child)?;
    Ok(child)
}
fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;
    Ok(())
}
fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= limit
            && path.canonicalize()? == path,
        "Attachment file is invalid or oversized"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file: File = options.open(path)?;
    ensure!(
        file.metadata()?.is_file() && !file.metadata()?.file_type().is_symlink(),
        "Attachment must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Attachment file is oversized");
    }
    Ok(bytes)
}
