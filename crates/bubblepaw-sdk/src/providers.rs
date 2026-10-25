//! Providers: the model request surface and the runner's adapters.

pub use bubblepaw_core::{
    Answer, AnswerKind, DecisionChoice, DecisionProvider, DecisionRequest, DecisionResponse,
    DecisionUsage, ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind,
    Question, QuestionKind, ReasoningEffort, TextCallback, Usage,
};
pub use bubblepaw_runner::{
    model_roles::ModelRole,
    provider::{
        ExtensionConfig, ExtensionProvider, ProviderEndpoints, ProviderRegistry, ResponsesProvider,
        ScriptedProvider, ScriptedStep,
    },
};
