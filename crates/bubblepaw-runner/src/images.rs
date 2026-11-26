//! Image bytes live in canonical attachments; messages carry small immutable references.
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bubblepaw_core::{
    CancellationToken, Capabilities, ContentBlock, Message, ModelInfo, ModelRequest, ModelResponse,
    Provider, Role, Store, TextCallback, text_blocks,
};
pub use bubblepaw_core::{ImageRef, MAX_IMAGE_BYTES, MAX_IMAGES_PER_CALL};
use serde_json::{Value, json};
use std::{collections::HashSet, fs::File, io::Read, path::Path, sync::Arc};

#[derive(Clone, Default)]
pub(crate) struct ImageBatch {
    images: Vec<ImageRef>,
    omitted: bool,
}
impl ImageBatch {
    pub(crate) fn merge(&mut self, other: &Self) {
        self.collect(other.images.clone());
        self.omitted |= other.omitted;
    }
    pub(crate) fn collect(&mut self, images: Vec<ImageRef>) {
        for image in images {
            if self.images.iter().any(|known| known.id == image.id) {
                continue;
            }
            if self.images.len() < MAX_IMAGES_PER_CALL {
                self.images.push(image);
            } else {
                self.omitted = true;
            }
        }
    }
    /// `content` as a result body: its text, the omission note when images were
    /// left out, then the collected images.
    pub(crate) fn blocks(&self, mut content: String) -> Vec<ContentBlock> {
        if self.omitted {
            content.push_str("\n[Image limit: at most 4 distinct images per tool call; additional images were omitted. Read them in another call.]");
        }
        blocks_with_images(content, &self.images)
    }
}

fn supported(mime: &str) -> bool {
    ImageRef::is_supported_mime(mime)
}

/// `text` followed by one image block per reference.
pub(crate) fn blocks_with_images(text: String, images: &[ImageRef]) -> Vec<ContentBlock> {
    let mut blocks = text_blocks(text);
    blocks.extend(images.iter().cloned().map(ContentBlock::image));
    blocks
}
pub(crate) fn mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

/// Sniff a bounded prefix first, so ordinary text retains the ordinary read path.
pub(crate) fn read_image(
    store: &Store,
    session: &str,
    path: &Path,
    relative: &str,
) -> Result<Option<Vec<ContentBlock>>> {
    let mut file = File::open(path)?;
    ensure!(file.metadata()?.is_file(), "path must be a regular file");
    let mut prefix = [0u8; 16];
    let count = file.read(&mut prefix)?;
    let Some(mime) = mime_type(&prefix[..count]) else {
        return Ok(None);
    };
    if file.metadata()?.len() > MAX_IMAGE_BYTES as u64 {
        return Ok(Some(text_blocks(json!({"path":relative,"imageOmitted":true,"note":"Image exceeds the 5 MB limit; no resize is performed. Use a smaller image."}).to_string())));
    }
    let mut bytes = prefix[..count].to_vec();
    file.take((MAX_IMAGE_BYTES + 1 - count) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Ok(Some(text_blocks(json!({"path":relative,"imageOmitted":true,"note":"Image exceeds the 5 MB limit; no resize is performed. Use a smaller image."}).to_string())));
    }
    let image = store_image(store, session, mime, &bytes)?;
    Ok(Some(blocks_with_images(json!({"path":relative,"image":image,"note":"Image attached to the tool result for image-capable providers."}).to_string(), &[image])))
}

pub fn store_image(store: &Store, session: &str, mime: &str, bytes: &[u8]) -> Result<ImageRef> {
    ensure!(
        supported(mime) && mime_type(bytes) == Some(mime),
        "unsupported image format"
    );
    ensure!(bytes.len() <= MAX_IMAGE_BYTES, "image exceeds 5 MB limit");
    let image = ImageRef {
        id: format!("image-{}", bubblepaw_core::fingerprint(bytes)),
        session_id: session.into(),
        mime_type: mime.into(),
        bytes: bytes.len(),
    };
    // Deterministic metadata excludes the mutable path, allowing identical bytes
    // read through several paths to share one attachment row and one blob.
    let metadata = serde_json::to_string(&image)?;
    if let Some((existing, _)) = store.attachment(session, &image.id)? {
        ensure!(existing == metadata, "image metadata mismatch");
    } else {
        store.store_attachment(session, &image.id, &metadata, bytes)?;
    }
    Ok(image)
}

/// The images of a hydrated message as `{mime_type, data}` values (data is
/// base64), in order. Blocks without bytes were not hydrated and are skipped.
pub(crate) fn wire_images(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image {
                image,
                data: Some(data),
            } => Some(json!({"mime_type":image.mime_type,"data":STANDARD.encode(data)})),
            _ => None,
        })
        .collect()
}

/// Fills the bytes of every tool-result image block, or replaces it with a
/// note when the bytes cannot be sent. Notes close the message's text.
pub(crate) fn hydrate_request(
    request: &mut ModelRequest,
    store: &Store,
    image_input: bool,
) -> Result<()> {
    let mut sent = HashSet::new();
    for message in &mut request.messages {
        if message.role != Role::Tool || message.images().next().is_none() {
            continue;
        }
        let mut notes = String::new();
        let mut blocks = Vec::new();
        for block in &message.content {
            let ContentBlock::Image { image, .. } = block else {
                blocks.push(block.clone());
                continue;
            };
            if !image_input {
                if notes.is_empty() {
                    notes.push_str("\n[Image input is unavailable for this model/provider; image bytes were not sent.]");
                }
                continue;
            }
            if sent.contains(&image.id) {
                notes.push_str("\n[Image already provided earlier in this request.]");
                continue;
            }
            let Some((metadata, bytes)) = store.attachment(&image.session_id, &image.id)? else {
                notes.push_str("\n[Image unavailable: canonical attachment was not found.]");
                continue;
            };
            let stored: ImageRef =
                serde_json::from_str(&metadata).context("invalid image attachment metadata")?;
            ensure!(
                &stored == image
                    && bytes.len() == image.bytes
                    && mime_type(&bytes) == Some(image.mime_type.as_str()),
                "image attachment mismatch"
            );
            ensure!(
                format!("image-{}", bubblepaw_core::fingerprint(&bytes)) == image.id,
                "image attachment integrity mismatch"
            );
            sent.insert(image.id.clone());
            blocks.push(ContentBlock::image_with_data(image.clone(), bytes));
        }
        if !notes.is_empty() {
            blocks.push(ContentBlock::Text { text: notes });
        }
        message.content = blocks;
    }
    Ok(())
}

/// Library hosts wrap their selected provider once; canonical messages keep image references.
pub struct ImageProvider {
    inner: Arc<dyn Provider>,
    store: Store,
}
impl ImageProvider {
    pub fn new(inner: Arc<dyn Provider>, store: Store) -> Self {
        Self { inner, store }
    }
}
#[async_trait::async_trait]
impl Provider for ImageProvider {
    fn summary_fork_routes(&self, model: &str) -> Vec<String> {
        self.inner.summary_fork_routes(model)
    }
    fn estimate_summary_fork_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        let mut request = request.clone();
        let image_input = self.inner.capabilities(&request.model).image_input;
        hydrate_request(&mut request, &self.store, image_input)?;
        self.inner.estimate_summary_fork_tokens(&request, count)
    }
    fn supports_summary_fork(&self, model: &str) -> bool {
        self.inner.supports_summary_fork(model)
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<Value>> {
        let mut request = request.clone();
        let image_input = self.inner.capabilities(&request.model).image_input;
        hydrate_request(&mut request, &self.store, image_input)?;
        self.inner.summary_fork_mapping(&request)
    }
    async fn generate_summary_fork(
        &self,
        mut request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let image_input = self.inner.capabilities(&request.model).image_input;
        hydrate_request(&mut request, &self.store, image_input)?;
        self.inner
            .generate_summary_fork(request, cancellation)
            .await
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        let mut request = request.clone();
        let image_input = self.inner.capabilities(&request.model).image_input;
        hydrate_request(&mut request, &self.store, image_input)?;
        self.inner.estimate_input_tokens(&request, count)
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<bubblepaw_core::WireShape> {
        // Hydrated images are part of the sent body, so shape the hydrated request.
        let mut request = request.clone();
        let image_input = self.inner.capabilities(&request.model).image_input;
        hydrate_request(&mut request, &self.store, image_input).ok()?;
        self.inner.wire_shape(&request)
    }
    fn models(&self) -> Vec<ModelInfo> {
        self.inner.models()
    }
    fn capabilities(&self, model: &str) -> Capabilities {
        self.inner.capabilities(model)
    }
    async fn generate(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let image_input = self.inner.capabilities(&request.model).image_input;
        hydrate_request(&mut request, &self.store, image_input)?;
        self.inner.generate(request, on_text, cancellation).await
    }
    async fn generate_decision(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let image_input = self.inner.capabilities(&request.model).image_input;
        hydrate_request(&mut request, &self.store, image_input)?;
        self.inner
            .generate_decision(request, on_text, cancellation)
            .await
    }
    async fn close_session(&self, session: &str) -> Result<()> {
        self.inner.close_session(session).await
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use crate::provider::ChatCompletionsProvider;
    use bubblepaw_core::{Message, ReasoningReplay, ToolCall};

    #[test]
    fn image_wrapper_projects_hydrated_input_and_preserves_native_canonical_state() -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("image projection")?.id;
        let image = store_image(
            &store,
            &session,
            "image/png",
            include_bytes!("../tests/fixtures/image-two-colors.png"),
        )?;
        let chat = Arc::new(
            ChatCompletionsProvider::without_key("http://127.0.0.1:1")?.with_reasoning_replay(
                std::collections::BTreeMap::from([("test".into(), ReasoningReplay::All)]),
            )?,
        );
        let wrapper = ImageProvider::new(chat.clone(), store.clone());
        let message = Message {
            id: "assistant".into(),
            session_id: session.clone(),
            role: Role::Assistant,
            origin: None,
            content: bubblepaw_core::text_blocks("Reading the image."),
            tool_calls: vec![ToolCall {
                id: "read-image".into(),
                name: "read".into(),
                arguments: json!({"path":"image.png"}),
            }],
            tool_call_id: None,
            provider_state: Some(
                json!({"adapter":"chat-completions","message_fields":{"reasoning_content":"native reasoning"}}),
            ),
            is_error: false,
            details: None,
            created_at: "2026-10-18T00:00:00Z".into(),
        };
        let request = ModelRequest {
            model: "test".into(),
            messages: vec![
                Message {
                    id: "user".into(),
                    role: Role::User,
                    content: bubblepaw_core::text_blocks("Inspect image.png"),
                    tool_calls: vec![],
                    provider_state: None,
                    ..message.clone()
                },
                message.clone(),
                Message {
                    id: "image-result".into(),
                    role: Role::Tool,
                    tool_calls: vec![],
                    tool_call_id: Some("read-image".into()),
                    provider_state: None,
                    content: blocks_with_images("image read".into(), &[image]),
                    ..message
                },
            ],
            ..Default::default()
        };
        let original = serde_json::to_value(&request)?;
        let mut hydrated = request.clone();
        hydrate_request(&mut hydrated, &store, true)?;
        let expected = std::cell::RefCell::new(Value::Null);
        chat.estimate_input_tokens(&hydrated, &|input| {
            *expected.borrow_mut() = input.clone();
            123
        })?;
        assert_eq!(
            wrapper.estimate_input_tokens(&request, &|input| {
                assert_eq!(input, &*expected.borrow());
                let wire = input.to_string();
                assert!(wire.contains("native reasoning"));
                assert!(wire.contains("data:image/png;base64,"));
                123
            })?,
            Some(123)
        );
        assert_eq!(serde_json::to_value(&request)?, original);
        Ok(())
    }
}
