//! The model adapters: chat completions, Anthropic messages and responses, with
//! the HTTP stream and error handling they share, the retry rule, request dumps
//! and the scripted model. Everything that talks HTTP is behind the `http`
//! feature; the scripted model, the replay-state helpers and the error helpers
//! build without it.
#[cfg(feature = "http")]
mod chat;
#[cfg(feature = "http")]
pub use chat::{ChatCompletionsProvider, is_loopback_url, response_usage as chat_response_usage};
#[cfg(feature = "http")]
mod continuation;
mod helpers;
pub use helpers::*;
#[cfg(feature = "http")]
mod helpers_http;
#[cfg(feature = "http")]
pub use helpers_http::*;
#[cfg(feature = "http")]
pub mod http_dump;
#[cfg(feature = "http")]
pub mod http_error;
#[cfg(feature = "http")]
mod images;
#[cfg(feature = "http")]
pub use images::wire_images;
#[cfg(feature = "http")]
mod messages;
#[cfg(feature = "http")]
pub use messages::MessagesProvider;
mod metering;
pub use metering::{PhysicalRequest, response_output_reported};
#[cfg(feature = "http")]
mod opencode_session;
#[cfg(feature = "http")]
pub use opencode_session::{codex_cache_headers, opencode_session, session_key};
#[cfg(feature = "http")]
pub mod quota_headers;
pub mod replay_state;
#[cfg(feature = "http")]
mod reply_facts;
#[cfg(feature = "http")]
mod responses;
#[cfg(feature = "http")]
mod responses_body;
#[cfg(feature = "http")]
mod responses_generate;
#[cfg(feature = "http")]
mod responses_rounds;
#[cfg(feature = "http")]
pub use responses::ResponsesProvider;
#[cfg(feature = "http")]
use responses_rounds::{is_length_stop, join_rounds};
#[cfg(feature = "http")]
mod retry;
#[cfg(feature = "http")]
pub use retry::{RetryPolicy, add_usage, with_retry};
mod scripted;
pub use scripted::{ScriptedProvider, ScriptedStep};
#[cfg(feature = "http")]
pub mod stream;
#[cfg(feature = "http")]
pub use stream::{EgressFailure, ModelEgress};
#[cfg(feature = "http")]
mod tool_wire;
#[cfg(feature = "http")]
pub use tool_wire::accepts_request_call;
#[cfg(feature = "http")]
use {
    crate::{
        CacheMode, CancellationToken, Capabilities, ModelInfo, ModelRequest, ModelResponse,
        Provider, ProviderError, ProviderErrorKind, Role, StreamCallback, StreamEvent, ToolCall,
        ToolSurface, Usage,
    },
    anyhow::{Context, Result, anyhow, ensure},
    async_trait::async_trait,
    serde_json::{Value, json},
    std::{
        collections::{BTreeMap, HashSet},
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, Ordering},
    },
    stream::{SseReader, TextBatch},
};
