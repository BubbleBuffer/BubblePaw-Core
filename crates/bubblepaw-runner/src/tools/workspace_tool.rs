use super::*;

/// Files one `read` call may name.
const MAX_READ_PATHS: usize = 50;
/// Room kept for a closing note inside the result limit.
const NOTE_ROOM: usize = 160;

pub(super) struct WorkspaceTool {
    pub(super) workspace: WorkspaceTools,
    pub(super) name: &'static str,
    pub(super) store: Store,
}
impl WorkspaceTool {
    async fn invoke_resolved(
        &self,
        context: ToolContext,
        input: Value,
        read_path: Option<(PathBuf, bool)>,
        plain: bool,
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
        if plain {
            return Ok(ToolResult::text(if name == "search" {
                search::render(&value)
            } else {
                read_text::render(&value)
            }));
        }
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

    /// One file: an image attachment, or the text of a line range.
    async fn read_one(
        &self,
        context: ToolContext,
        input: Value,
        plain: bool,
        allow_images: bool,
    ) -> Result<ToolResult> {
        validate_fields(&input, &["path", "startLine", "lineCount"])?;
        let relative = string(&input, "path")?.to_owned();
        ensure!(
            count(&input, "startLine", 1)? > 0 && count(&input, "lineCount", 1000)? > 0,
            "startLine and lineCount must be positive"
        );
        let pads = self
            .workspace
            .scratchpads(&self.store, &context.session_id)?;
        let read_path = match pads.resolve_from(self.workspace.root(), &relative, false)? {
            Some(path) => (path, false),
            None => {
                self.workspace
                    .readable(&relative, &context.session_id, self.store.data_dir())?
            }
        };
        let path = read_path.0.clone();
        let store = self.store.clone();
        let session = context.session_id.clone();
        let cancellation = context.cancellation.clone();
        if let Some(content) = tokio::task::spawn_blocking(move || {
            ensure!(!cancellation.is_cancelled(), "cancelled");
            crate::images::read_image(&store, &session, &path, &relative)
        })
        .await??
        {
            ensure!(
                allow_images,
                "images are read one at a time: call read with path for this file"
            );
            return Ok(ToolResult::blocks(content));
        }
        self.invoke_resolved(context, input, Some(read_path), plain)
            .await
    }

    /// Several files in one call, each under a header, within one size cap.
    async fn read_many(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        ensure!(
            input.get("path").is_none(),
            "pass either path or paths, not both"
        );
        let entries = input["paths"]
            .as_array()
            .ok_or_else(|| anyhow!("paths must be an array"))?;
        ensure!(
            (1..=MAX_READ_PATHS).contains(&entries.len()),
            "paths must hold 1 to {MAX_READ_PATHS} entries"
        );
        let budget = MAX_RESULT - 1024;
        let mut out = String::new();
        let (mut cut, mut skipped) = (Vec::new(), Vec::new());
        let mut full = false;
        for entry in entries {
            ensure!(!context.cancellation.is_cancelled(), "cancelled");
            let mut single = match entry {
                Value::String(path) => json!({"path":path}),
                Value::Object(_) => {
                    validate_fields(entry, &["path", "startLine", "lineCount"])?;
                    entry.clone()
                }
                _ => bail!("paths entries must be strings or objects"),
            };
            let path = string(&single, "path")?.to_owned();
            for key in ["startLine", "lineCount"] {
                if single.get(key).is_none()
                    && let Some(default) = input.get(key)
                {
                    single[key] = default.clone();
                }
            }
            let start = count(&single, "startLine", 1).unwrap_or(1);
            let header = format!("=== {path} ===\n");
            if full || out.len() + header.len() + 256 > budget {
                full = true;
                skipped.push(path);
                continue;
            }
            let mut text = match self.read_one(context.clone(), single, true, false).await {
                Ok(result) => result.body().into_owned(),
                Err(error) => format!("[error: {error:#}]"),
            };
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let room = budget - out.len() - header.len();
            if text.len() > room {
                let (head, _) = clip(&text, room.saturating_sub(NOTE_ROOM));
                let end = head.rfind('\n').map_or(0, |end| end + 1);
                let shown = &text[..end];
                let next = start + shown.matches('\n').count();
                text = format!(
                    "{shown}[cut at the total size cap; read this file alone with startLine={next}]\n"
                );
                cut.push(path.clone());
                full = true;
            }
            out.push_str(&header);
            out.push_str(&text);
        }
        if !cut.is_empty() || !skipped.is_empty() {
            let mut note = String::from("[total size cap reached.");
            if !cut.is_empty() {
                note.push_str(&format!(" Cut: {}.", cut.join(", ")));
            }
            if !skipped.is_empty() {
                note.push_str(&format!(" Not read: {}.", skipped.join(", ")));
            }
            note.push_str(" Read them in a further call.]");
            out.push_str(&note);
        }
        Ok(ToolResult::text(out))
    }
}
#[async_trait]
impl Tool for WorkspaceTool {
    fn definition(&self) -> ToolDefinition {
        let (description, properties, required) = match self.name {
            "read" => (
                "Read UTF-8 text (a partial read ends with a [lines a-b of n] note) or attach a PNG, JPEG, GIF or WebP image. Images are limited to 5 MB without resizing. startLine is 1-based; lineCount defaults to 1000 (maximum 2000). To read several files in one call pass paths instead of path: an array of paths or {path,startLine,lineCount} objects (startLine and lineCount beside it are the defaults); each file comes back under a === path === header within one total size cap, and a closing note names the files that were cut or skipped. Prefer one call with paths over one call per file; images are read one at a time with path. Absolute paths in the chat and project scratchpads are also allowed. This chat's spill directory and listed skill folders are read-only exceptions; spill files support line ranges up to the 256 MiB retention cap.",
                json!({"path":{"type":"string"},"paths":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"object","properties":{"path":{"type":"string"},"startLine":{"type":"integer","minimum":1},"lineCount":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false}]}},"startLine":{"type":"integer","minimum":1},"lineCount":{"type":"integer","minimum":1}}),
                vec![],
            ),
            "search" => (
                "Find files and text in the workspace, recursively; one call answers what is in a folder or repo and where something is. Without query it lists the files under path (default the workspace root) with their sizes. With query it returns the matching lines as path:line: text (literal text, or a regex when regex is true). glob keeps only matching files: a .gitignore-style pattern or an array of them, a leading ! excludes (for example \"*.rs\", [\"src/**/*.ts\",\"!*.test.ts\"]). context adds that many lines around each match (maximum 10); filesOnly returns just the paths of files that contain a match. Honours .gitignore; skips symlinks and .git, node_modules and target. Results stop at maxResults (default 500 files or 100 matches; maximum 5000 and 1000) and end with a note saying how many more there are.",
                json!({"path":{"type":"string"},"query":{"type":"string"},"glob":{"oneOf":[{"type":"string"},{"type":"array","items":{"type":"string"}}]},"regex":{"type":"boolean"},"context":{"type":"integer","minimum":0},"filesOnly":{"type":"boolean"},"maxResults":{"type":"integer","minimum":1}}),
                vec![],
            ),
            "edit" => (
                "Replace literal text in an existing workspace file or an absolute path in the chat or project scratchpad. Copy old_string exactly from a fresh read, including indentation, with enough surrounding lines to match once; use replace_all only to replace every occurrence. Ignores CRLF/LF and BOM differences; keeps line endings. No regex. For several edits use edits (same fields) instead of old_string/new_string: in order, all or nothing. Use write to create a file.",
                json!({"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"},"edits":{"type":"array","items":{"type":"object","properties":{"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["old_string","new_string"]}}}),
                vec!["path"],
            ),
            "write" => (
                "Create or overwrite a workspace file or an absolute path in the chat or project scratchpad, creating missing parent directories. Supply complete UTF-8 content; use edit for targeted changes to an existing file.",
                json!({"path":{"type":"string"},"content":{"type":"string"}}),
                vec!["path", "content"],
            ),
            _ => unreachable!("unknown workspace tool"),
        };
        ToolDefinition {
            approval: Default::default(),
            read_only: matches!(self.name, "read" | "search"),
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
            let mut input = input;
            let store = self.store.clone();
            let pads = self.workspace.scratchpads(&store, &context.session_id)?;
            let requested = string(&input, "path")?;
            let requested_path = self.workspace.root.join(requested);
            let resolved = pads.resolve_from(self.workspace.root(), requested, true)?;
            let (workspace, scratch_path) = if let Some(path) = resolved {
                ensure!(
                    !fs::symlink_metadata(&requested_path)
                        .is_ok_and(|metadata| metadata.file_type().is_symlink()),
                    "refusing to replace a symlink"
                );
                let workspace = self.workspace.scratch_mutation(pads, &path)?;
                input["path"] = json!(
                    path.strip_prefix(workspace.root())?
                        .to_str()
                        .context("path is not UTF-8")?
                );
                (workspace, Some(path))
            } else {
                (self.workspace.clone(), None)
            };
            let mut value = tokio::task::spawn_blocking(move || {
                let _guard = workspace.mutation_guard()?;
                workspace.edit_or_write_checkpointed(name, &input, &context, &store)
            })
            .await??;
            if let Some(path) = scratch_path {
                value["path"] = json!(path);
            }
            let result = ToolResult::text(serde_json::to_string(&value)?);
            // The edit summary (path, replacements) is also data for interfaces.
            return Ok(if name == "edit" {
                result.with_details(value)
            } else {
                result
            });
        }
        if self.name == "read" {
            validate_fields(&input, &["path", "paths", "startLine", "lineCount"])?;
            if input.get("paths").is_some() {
                return self.read_many(context, input).await;
            }
            return self
                .read_one(context, input, dispatcher.origin() != "js", true)
                .await;
        }
        if self.name == "search" && dispatcher.origin() != "js" {
            return self.invoke_resolved(context, input, None, true).await;
        }
        self.invoke(context, input).await
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        ensure!(
            !matches!(self.name, "edit" | "write"),
            "{} requires the tool dispatcher to persist file checkpoints",
            self.name
        );
        self.invoke_resolved(context, input, None, false).await
    }
}
