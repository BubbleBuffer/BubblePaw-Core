//! Context preparation: the replaceable serving-view hook and the runner's summarizer.

pub use bubblepaw_core::{
    ContextPreparation, ContextWriter, PrepareCall, PrepareMode, PreparedContext,
};
pub use bubblepaw_runner::summarization::{
    SummarizingContext, SummaryConfig, cached_context_window,
};
