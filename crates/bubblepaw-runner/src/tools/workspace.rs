use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use bubblepaw_core::{Tool, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock, Weak},
};

#[path = "edit.rs"]
mod edit;
#[path = "files.rs"]
mod files;

const MAX_FILE: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_MUTATION_FILE: u64 = 32 * 1024 * 1024;
const MAX_PREVIEW: usize = 64 * 1024;
const MAX_RESULT: usize = 60 * 1024;

// All views of a canonical workspace, including host rewind, share mutation
// exclusion. Weak entries avoid keeping abandoned workspace locks alive.
fn mutation_lock(root: &Path) -> Result<Arc<Mutex<()>>> {
    static LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| anyhow!("workspace lock registry poisoned"))?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(root.to_owned(), Arc::downgrade(&lock));
    Ok(lock)
}

#[derive(Clone)]
pub struct WorkspaceTools {
    root: PathBuf,
    mutation: Arc<Mutex<()>>,
    skill_read_roots: Arc<RwLock<BTreeMap<String, Vec<PathBuf>>>>,
    spill_data_dir: Option<PathBuf>,
}
impl WorkspaceTools {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let requested = root.as_ref();
        let root = fs::canonicalize(requested)
            .with_context(|| format!("workspace root does not exist: {}", requested.display()))?;
        ensure!(root.is_dir(), "workspace root must be a directory");
        Ok(Self {
            mutation: mutation_lock(&root)?,
            root,
            skill_read_roots: Arc::default(),
            spill_data_dir: None,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub(crate) fn mutation_guard(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.mutation
            .lock()
            .map_err(|_| anyhow!("workspace mutation lock poisoned"))
    }
    /// Keep session read grants across workspace views; writes use a new lock.
    pub(crate) fn for_workspace(&self, root: impl AsRef<Path>) -> Result<Self> {
        let mut workspace = Self::new(root)?;
        workspace.skill_read_roots = self.skill_read_roots.clone();
        workspace.spill_data_dir = self.spill_data_dir.clone();
        Ok(workspace)
    }
    pub(crate) fn with_spill_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.spill_data_dir = Some(data_dir);
        self
    }
    /// Grant read-only access to folders whose skills were listed to this session.
    pub fn set_skill_read_roots(&self, session: &str, roots: Vec<PathBuf>) -> Result<()> {
        for root in &roots {
            ensure!(
                root.is_absolute()
                    && !root
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir)),
                "skill read roots must already be canonical absolute paths"
            );
        }
        // The skill inventory supplies canonical identities. Preserve them after
        // removal or ancestor replacement; canonicalizing again could grant a
        // new symlink target the session has never been shown.
        self.skill_read_roots
            .write()
            .map_err(|_| anyhow!("skill read roots poisoned"))?
            .insert(session.to_owned(), roots);
        Ok(())
    }
    fn readable(
        &self,
        relative: &str,
        session: &str,
        data_dir: Option<&Path>,
    ) -> Result<(PathBuf, bool)> {
        let path = self
            .root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("read path does not exist: {relative}"))?;
        let data_dir = data_dir
            .or(self.spill_data_dir.as_deref())
            .unwrap_or(&self.root);
        let spill_root = data_dir.join("spill");
        if path.starts_with(&spill_root) || Path::new(relative).starts_with(&spill_root) {
            let spills = crate::spill::SpillStore::new(data_dir)?;
            return Ok((spills.readable(session, &self.root.join(relative))?, true));
        }
        if path.starts_with(&self.root) {
            return Ok((path, false));
        }
        let roots = self
            .skill_read_roots
            .read()
            .map_err(|_| anyhow!("skill read roots poisoned"))?;
        ensure!(
            roots
                .get(session)
                .is_some_and(|roots| roots.iter().any(|root| path.starts_with(root))),
            "path escapes workspace and this session's read grants"
        );
        Ok((path, false))
    }
    pub fn register(&self, registry: &mut ToolRegistry, read_only: bool) -> Result<()> {
        for name in ["list", "read", "search", "edit", "write"] {
            if read_only && matches!(name, "edit" | "write") {
                continue;
            }
            registry.register_host(Arc::new(WorkspaceTool {
                workspace: self.clone(),
                name,
            }))?;
        }
        Ok(())
    }
    pub(crate) fn existing(&self, relative: &str) -> Result<PathBuf> {
        let resolved = fs::canonicalize(self.root.join(relative))
            .with_context(|| format!("workspace path does not exist: {relative}"))?;
        ensure!(
            resolved.starts_with(&self.root),
            "path escapes workspace root"
        );
        Ok(resolved)
    }
    fn destination(&self, relative: &str) -> Result<PathBuf> {
        let path = self.root.join(relative);
        let mut ancestor = path
            .parent()
            .ok_or_else(|| anyhow!("path needs a parent"))?;
        let mut missing = Vec::new();
        while !ancestor.exists() {
            ensure!(
                fs::symlink_metadata(ancestor).is_err(),
                "destination parent is inaccessible or a dangling symlink"
            );
            missing.push(
                ancestor
                    .file_name()
                    .ok_or_else(|| anyhow!("invalid destination parent"))?
                    .to_owned(),
            );
            ancestor = ancestor
                .parent()
                .ok_or_else(|| anyhow!("path needs an existing ancestor"))?;
        }
        let mut parent = fs::canonicalize(ancestor)?;
        ensure!(
            parent.starts_with(&self.root) && parent.is_dir(),
            "path escapes workspace root"
        );
        for component in missing.into_iter().rev() {
            parent.push(component);
        }
        let destination = parent.join(
            path.file_name()
                .ok_or_else(|| anyhow!("path needs a filename"))?,
        );
        ensure!(
            !fs::symlink_metadata(&destination).is_ok_and(|m| m.file_type().is_symlink()),
            "refusing to replace a symlink"
        );
        if destination.exists() {
            self.existing(relative)
        } else {
            Ok(destination)
        }
    }
    fn execute(
        &self,
        name: &str,
        input: Value,
        cancellation: bubblepaw_core::CancellationToken,
        session: &str,
        read_path: Option<(PathBuf, bool)>,
    ) -> Result<Value> {
        let fields: &[&str] = match name {
            "list" => &["path", "limit"],
            "read" => &["path", "startLine", "lineCount"],
            "search" => &["path", "query", "regex", "maxResults"],
            "edit" => &["path", "old_string", "new_string", "replace_all"],
            "write" => &["path", "content"],
            _ => bail!("unknown workspace tool"),
        };
        validate_fields(&input, fields)?;
        let relative = optional_string(&input, "path")?.unwrap_or(".");
        if name == "read" {
            string(&input, "path")?;
        }
        match name {
            "list" => {
                let directory = self.existing(relative)?;
                ensure!(directory.is_dir(), "path is not a directory");
                let limit = count(&input, "limit", 200)?.min(1000);
                ensure!(limit > 0, "limit must be positive");
                let mut entries = Vec::new();
                let mut truncated = false;
                for entry in fs::read_dir(&directory)? {
                    ensure!(!cancellation.is_cancelled(), "cancelled");
                    let entry = entry?;
                    let metadata = entry.path().symlink_metadata()?;
                    let value = json!({"name":entry.file_name().to_string_lossy(),"kind":if metadata.file_type().is_symlink(){"symlink"}else if metadata.is_dir(){"directory"}else{"file"},"bytes":metadata.len()});
                    let index = entries
                        .binary_search_by(|v: &Value| {
                            v["name"]
                                .as_str()
                                .unwrap()
                                .cmp(value["name"].as_str().unwrap())
                        })
                        .unwrap_or_else(|index| index);
                    if index < limit {
                        entries.insert(index, value);
                    }
                    if entries.len() > limit {
                        entries.pop();
                        truncated = true;
                    } else if index >= limit {
                        truncated = true;
                    }
                }
                Ok(json!({"path":relative,"entries":entries,"truncated":truncated}))
            }
            "read" => {
                let (path, spill) = match read_path {
                    Some(path) => path,
                    None => self.readable(relative, session, None)?,
                };
                let start = count(&input, "startLine", 1)?;
                let limit = count(&input, "lineCount", 1000)?.min(2000);
                ensure!(
                    start > 0 && limit > 0,
                    "startLine and lineCount must be positive"
                );
                read_lines(
                    &path,
                    relative,
                    start,
                    limit,
                    if spill {
                        crate::spill::CHAT_LIMIT_BYTES
                    } else {
                        MAX_FILE
                    },
                    &cancellation,
                )
            }
            "search" => {
                let root = self.existing(relative)?;
                let query = string(&input, "query")?;
                let regex_mode = match input.get("regex") {
                    None => false,
                    Some(value) => value
                        .as_bool()
                        .ok_or_else(|| anyhow!("regex must be a boolean"))?,
                };
                let pattern = if regex_mode {
                    query.to_owned()
                } else {
                    regex::escape(query)
                };
                let regex = Regex::new(&pattern).context("invalid search pattern")?;
                let limit = count(&input, "maxResults", 100)?.min(1000);
                ensure!(limit > 0, "maxResults must be positive");
                let mut pending = vec![root];
                let mut matches = Vec::new();
                let mut visited = 0usize;
                let mut truncated = false;
                while let Some(path) = pending.pop() {
                    ensure!(!cancellation.is_cancelled(), "cancelled");
                    visited += 1;
                    if visited > 20_000 || matches.len() >= limit {
                        truncated = true;
                        break;
                    }
                    let metadata = path.symlink_metadata()?;
                    if metadata.file_type().is_symlink() {
                        continue;
                    }
                    if metadata.is_dir() {
                        if path.file_name().is_some_and(|n| {
                            [".git", "node_modules", "target"]
                                .iter()
                                .any(|skip| n == *skip)
                        }) {
                            continue;
                        }
                        let remaining = 20_000usize.saturating_sub(pending.len() + visited);
                        let mut children = BTreeSet::new();
                        for entry in fs::read_dir(path)? {
                            ensure!(!cancellation.is_cancelled(), "cancelled");
                            children.insert(entry?.path());
                            if children.len() > remaining {
                                children.pop_last();
                                truncated = true;
                            }
                        }
                        pending.extend(children.into_iter().rev());
                    } else if metadata.is_file() && metadata.len() <= 2 * 1024 * 1024 {
                        let Ok(bytes) = read_bounded(&path) else {
                            continue;
                        };
                        if bytes.len() > 2 * 1024 * 1024 {
                            continue;
                        }
                        let Ok(text) = std::str::from_utf8(&bytes) else {
                            continue;
                        };
                        for (index, line) in text.lines().enumerate() {
                            if regex.is_match(line) {
                                matches.push(json!({"path":path.strip_prefix(&self.root)?.to_string_lossy(),"line":index+1,"text":clip(line,2000).0}));
                                if matches.len() >= limit {
                                    truncated = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                Ok(json!({"matches":matches,"truncated":truncated,"visitedPaths":visited}))
            }
            "edit" | "write" => {
                bail!("{name} requires the tool dispatcher to persist file checkpoints")
            }
            _ => bail!("unknown workspace tool"),
        }
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    ensure!(path.metadata()?.is_file(), "path must be a regular file");
    ensure!(
        path.metadata()?.len() <= MAX_FILE,
        "file exceeds 8 MiB limit"
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_FILE, "file exceeds 8 MiB limit");
    Ok(bytes)
}
fn read_lines(
    path: &Path,
    label: &str,
    start: usize,
    limit: usize,
    max_file: u64,
    cancellation: &bubblepaw_core::CancellationToken,
) -> Result<Value> {
    let file = fs::File::open(path)?;
    ensure!(file.metadata()?.is_file(), "path must be a regular file");
    ensure!(
        file.metadata()?.len() <= max_file,
        "file exceeds {} byte limit",
        max_file
    );
    let mut reader = file.take(max_file + 1);
    let mut buffer = [0u8; 32 * 1024];
    let mut selected = Vec::new();
    let mut line = 1usize;
    let mut total_bytes = 0u64;
    let mut last = None;
    let mut clipped = false;
    loop {
        ensure!(!cancellation.is_cancelled(), "cancelled");
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total_bytes += count as u64;
        ensure!(
            total_bytes <= max_file,
            "file exceeds {} byte limit",
            max_file
        );
        for &byte in &buffer[..count] {
            if line >= start && line < start.saturating_add(limit) {
                if selected.len() < MAX_PREVIEW {
                    selected.push(byte);
                } else {
                    clipped = true;
                }
            }
            if byte == b'\n' {
                line += 1;
            }
            last = Some(byte);
        }
    }
    let total_lines = if last.is_none() {
        0
    } else {
        line - usize::from(last == Some(b'\n'))
    };
    let content = match std::str::from_utf8(&selected) {
        Ok(content) => content,
        Err(error) if clipped && error.error_len().is_none() => {
            std::str::from_utf8(&selected[..error.valid_up_to()])?
        }
        Err(error) => return Err(error).context("file is not UTF-8"),
    };
    Ok(
        json!({"path":label,"content":content,"startLine":start,"totalLines":total_lines,"returnedLines":content.split_inclusive('\n').count(),"truncated":clipped || start.saturating_sub(1).saturating_add(limit)<total_lines}),
    )
}
pub(crate) fn string<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string field {key}"))
}
pub(crate) fn optional_string<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>> {
    input
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| anyhow!("{key} must be a string"))
        })
        .transpose()
}
pub(crate) fn unsigned(input: &Value, key: &str, default: Option<u64>) -> Result<Option<u64>> {
    match input.get(key) {
        None => Ok(default),
        Some(value) => {
            Ok(Some(value.as_u64().ok_or_else(|| {
                anyhow!("{key} must be a nonnegative integer")
            })?))
        }
    }
}
pub(crate) fn count(input: &Value, key: &str, default: usize) -> Result<usize> {
    Ok(unsigned(input, key, Some(default as u64))?
        .unwrap()
        .min(usize::MAX as u64) as usize)
}
pub(crate) fn validate_fields(input: &Value, fields: &[&str]) -> Result<()> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("input must be an object"))?;
    for key in object.keys() {
        ensure!(
            fields.contains(&key.as_str()),
            "unexpected field {key}; valid arguments: {}",
            if fields.is_empty() {
                "(none)".to_owned()
            } else {
                fields.join(", ")
            }
        );
    }
    Ok(())
}
pub(crate) fn clip(text: &str, limit: usize) -> (&str, bool) {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], end < text.len())
}
struct WorkspaceTool {
    workspace: WorkspaceTools,
    name: &'static str,
}
impl WorkspaceTool {
    async fn invoke_resolved(
        &self,
        context: ToolContext,
        input: Value,
        read_path: Option<(PathBuf, bool)>,
    ) -> Result<ToolResult> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        let workspace = self.workspace.clone();
        let name = self.name;
        let mut value = tokio::task::spawn_blocking(move || {
            workspace.execute(
                name,
                input,
                context.cancellation,
                &context.session_id,
                read_path,
            )
        })
        .await??;
        // Keep text pages bounded; oversized listings/searches flow through the
        // dispatcher's spill policy instead of losing entries before dispatch.
        if serde_json::to_vec(&value)?.len() > MAX_RESULT
            && let Some(content) = value["content"].as_str()
        {
            let content = clip(content, 4096).0.to_owned();
            value["returnedLines"] = json!(content.split_inclusive('\n').count());
            value["content"] = json!(content);
            value["truncated"] = json!(true);
        }
        Ok(ToolResult::text(serde_json::to_string(&value)?))
    }
}
#[async_trait]
impl Tool for WorkspaceTool {
    fn definition(&self) -> ToolDefinition {
        let (description, properties, required) = match self.name {
            "list" => (
                "List a workspace directory in name order, then apply limit (default 200, maximum 1000).",
                json!({"path":{"type":"string"},"limit":{"type":"integer","minimum":1}}),
                vec![],
            ),
            "read" => (
                "Read UTF-8 lines or attach a PNG, JPEG, GIF or WebP image to the model. Images are limited to 5 MB without resizing. startLine is 1-based; lineCount defaults to 1000 (maximum 2000). Text previews are bounded to 64 KiB. Absolute paths in this chat's spill directory and listed skill folders are read-only exceptions to the workspace boundary; spill files support line ranges up to the 256 MiB retention cap.",
                json!({"path":{"type":"string"},"startLine":{"type":"integer","minimum":1},"lineCount":{"type":"integer","minimum":1}}),
                vec!["path"],
            ),
            "search" => (
                "Search workspace text; skips symlinks and .git, node_modules, target.",
                json!({"path":{"type":"string"},"query":{"type":"string"},"regex":{"type":"boolean"},"maxResults":{"type":"integer","minimum":1}}),
                vec!["query"],
            ),
            "edit" => (
                "Replace literal text in an existing workspace file. Copy old_string exactly from a fresh read, including indentation, with enough surrounding lines to match once; use replace_all only to replace every occurrence. No regex or whitespace normalization. For several edits, call edit several times. Use write to create a file.",
                json!({"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}}),
                vec!["path", "old_string", "new_string"],
            ),
            "write" => (
                "Create or overwrite a workspace file with complete UTF-8 content, creating missing parent directories. Supply the complete content; use edit for targeted changes to an existing file.",
                json!({"path":{"type":"string"},"content":{"type":"string"}}),
                vec!["path", "content"],
            ),
            _ => unreachable!("unknown workspace tool"),
        };
        ToolDefinition {
            approval: Default::default(),
            read_only: matches!(self.name, "list" | "read" | "search"),
            name: self.name.into(),
            description: description.into(),
            input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        }
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        if matches!(self.name, "edit" | "write") {
            let name = self.name;
            let workspace = self.workspace.clone();
            let store = dispatcher.store().clone();
            let value = tokio::task::spawn_blocking(move || {
                let _guard = workspace.mutation_guard()?;
                workspace.edit_or_write_checkpointed(name, &input, &context, &store)
            })
            .await??;
            return Ok(ToolResult::text(serde_json::to_string(&value)?));
        }
        if self.name == "read" {
            validate_fields(&input, &["path", "startLine", "lineCount"])?;
            let relative = string(&input, "path")?.to_owned();
            ensure!(
                count(&input, "startLine", 1)? > 0 && count(&input, "lineCount", 1000)? > 0,
                "startLine and lineCount must be positive"
            );
            let read_path = self.workspace.readable(
                &relative,
                &context.session_id,
                dispatcher.store().data_dir(),
            )?;
            let path = read_path.0.clone();
            let store = dispatcher.store().clone();
            let session = context.session_id.clone();
            let cancellation = context.cancellation.clone();
            if let Some(content) = tokio::task::spawn_blocking(move || {
                ensure!(!cancellation.is_cancelled(), "cancelled");
                crate::images::read_image(&store, &session, &path, &relative)
            })
            .await??
            {
                return Ok(ToolResult::text(content));
            }
            return self.invoke_resolved(context, input, Some(read_path)).await;
        }
        self.invoke(context, input).await
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        ensure!(
            !matches!(self.name, "edit" | "write"),
            "{} requires the tool dispatcher to persist file checkpoints",
            self.name
        );
        self.invoke_resolved(context, input, None).await
    }
}
