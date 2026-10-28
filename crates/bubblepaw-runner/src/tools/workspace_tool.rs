use super::*;

pub(super) struct WorkspaceTool {
    pub(super) workspace: WorkspaceTools,
    pub(super) name: &'static str,
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
                "Read UTF-8 lines or attach a PNG, JPEG, GIF or WebP image to the model. Images are limited to 5 MB without resizing. startLine is 1-based; lineCount defaults to 1000 (maximum 2000). Text previews are bounded to 64 KiB. Absolute paths in the chat and project scratchpads are also allowed. This chat's spill directory and listed skill folders are read-only exceptions; spill files support line ranges up to the 256 MiB retention cap.",
                json!({"path":{"type":"string"},"startLine":{"type":"integer","minimum":1},"lineCount":{"type":"integer","minimum":1}}),
                vec!["path"],
            ),
            "search" => (
                "Search workspace text; skips symlinks and .git, node_modules, target.",
                json!({"path":{"type":"string"},"query":{"type":"string"},"regex":{"type":"boolean"},"maxResults":{"type":"integer","minimum":1}}),
                vec!["query"],
            ),
            "edit" => (
                "Replace literal text in an existing workspace file or an absolute path in the chat or project scratchpad. Copy old_string exactly from a fresh read, including indentation, with enough surrounding lines to match once; use replace_all only to replace every occurrence. No regex or whitespace normalization. For several edits, call edit several times. Use write to create a file.",
                json!({"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}}),
                vec!["path", "old_string", "new_string"],
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
            let mut input = input;
            let store = dispatcher.store().clone();
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
            return Ok(ToolResult::text(serde_json::to_string(&value)?));
        }
        if self.name == "read" {
            validate_fields(&input, &["path", "startLine", "lineCount"])?;
            let relative = string(&input, "path")?.to_owned();
            ensure!(
                count(&input, "startLine", 1)? > 0 && count(&input, "lineCount", 1000)? > 0,
                "startLine and lineCount must be positive"
            );
            let pads = self
                .workspace
                .scratchpads(dispatcher.store(), &context.session_id)?;
            let read_path = match pads.resolve_from(self.workspace.root(), &relative, false)? {
                Some(path) => (path, false),
                None => self.workspace.readable(
                    &relative,
                    &context.session_id,
                    dispatcher.store().data_dir(),
                )?,
            };
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
