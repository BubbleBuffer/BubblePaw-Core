//! Model-facing types and the `Provider` trait.
//!
//! This is the bottom layer of the workspace (docs/layering.md): the messages,
//! requests, replies and streaming events that pass between a harness and a
//! model, and the trait a model adapter implements. It depends on no other
//! workspace crate and knows no store, run or tool registry.
pub mod adapter;
mod branch;
mod cancellation;
pub mod catalog;
mod content_block;
#[cfg(feature = "extension")]
pub mod extension;
mod hash;
mod incomplete;
mod message;
mod model_response;
mod output_schema;
mod provider;
mod provider_ext;
mod quota;
mod quota_host;
mod reasoning;
mod request;
mod request_account;
mod request_size;
mod router;
mod router_generate;
mod router_observer;
mod router_reply;
mod router_stream;
mod stream_event;
mod usage;
mod usage_sink;
mod wire_shape;

pub use branch::{Branch, BranchView};
pub use cancellation::CancellationToken;
pub use content_block::{
    ContentBlock, ImageRef, InputFileRef, MAX_IMAGE_BYTES, MAX_IMAGES_PER_CALL, MessageInput,
    content_body_text, content_files, content_images, content_text, content_thinking,
    file_envelope, plain_content, reply_blocks, text_blocks, with_text,
};
#[doc(hidden)]
pub use content_block::{extra_content, upgrade_user_content, wire};
pub use hash::{fingerprint, hex_lower};
pub use incomplete::IncompleteReason;
pub use message::{Message, MessageOrigin, Role, is_typed_user};
pub use model_response::{BREAKPOINTS_KEY, ModelResponse, ServedRoute, StopReason};
pub use output_schema::JsonSchema;
pub use provider::Provider;
pub use provider_ext::{
    CacheMode, Capabilities, CursorResets, EPHEMERAL_ID_PREFIX, HistoryCursor, HistoryTracker,
    ProviderError, ProviderErrorKind, ToolSurface, is_ephemeral, provider_error_retryable,
};
pub use quota::{QuotaReading, QuotaSource, QuotaWait};
pub use quota_host::{QuotaStop, SHORT_WAIT, is_quota_stop};
pub use reasoning::{
    PROVIDER_STATE_ORIGIN_FIELD, ReasoningReplay, provider_state_replay_compatible,
    with_provider_state_origin,
};
pub use request::{
    Approval, CacheTiers, ModelInfo, ModelRequest, ReasoningEffort, ToolCall, ToolDefinition,
};
pub use request_account::{current_account, with_request_usage_account};
#[doc(hidden)]
pub use request_size::prepared_message_bytes;
pub use request_size::prepared_request_bytes;
pub use router::Router;
pub use router_observer::{BoxFuture, NoObserver, RetryLoop, RouteObserver, RouteRef};
pub use router_reply::{FailedAttempts, with_usage};
pub use stream_event::{StreamCallback, StreamEvent, text_callback};
pub use usage::{LastRequestUsage, Usage};
pub use usage_sink::{AttemptReport, UsageSink, current_request_kind, with_usage_sink};
pub use wire_shape::{DEFAULT_CACHE_BLOCK_TOKENS, RequestKind, WireShape};

pub(crate) fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
