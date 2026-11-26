//! Provider wiring. The model adapters live in `bubblepaw-ai`; the registry, the
//! live configuration, the extension provider and the decision adapters stay here.
mod decisions;
mod endpoints;
mod extension;
mod live_provider;
mod metering;
mod registry;
mod registry_stream;
pub use bubblepaw_ai::adapter::{
    ChatCompletionsProvider, MessagesProvider, ResponsesProvider, RetryPolicy, ScriptedProvider,
    ScriptedStep, is_terminal_quota_error, parse_retry_after, provider_retry_after,
};
pub(crate) use bubblepaw_ai::adapter::{
    PhysicalRequest, add_usage, cancelled, current_rejection_quota_count, is_loopback_url,
    replay_state, with_retry,
};
use bubblepaw_ai::adapter::{attach_retry_after, classify, terminal_quota_failure, validate_url};
use bubblepaw_ai::adapter::{http_dump, http_error, stream};
use bubblepaw_ai::adapter::{mark_published, retain_prior_quota, track_text_publication};
pub(crate) use decisions::with_decision_usage;
pub use decisions::{
    LlmDecisions, OpenAiDecisions, OpenCodeDecisions, OpenRouterDecisions, decision_error_usage,
};
pub use endpoints::{ProviderEndpoints, ProviderKeys};
pub use extension::{
    EXTENSION_ENV_KEYS, ExtensionConfig, ExtensionProvider, load_extensions,
    valid_env_name as valid_extension_env_name,
};
pub use live_provider::{
    LiveProviderConfig, PreparedProviderConfig, ProviderGeneration, build_provider_registry,
};
pub use registry::ProviderRegistry;
