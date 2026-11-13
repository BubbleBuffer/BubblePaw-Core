//! Admission is repeated after appending recovery controls so a retry cannot
//! exceed the captured fork's byte or physical context budgets.
use super::*;

pub(super) struct ForkSize {
    pub input_bytes: usize,
    pub tokens: usize,
    pub output_reserve: usize,
    pub window: Option<usize>,
}

pub(super) fn admit_fork(
    models: &HttpEpisodeModels,
    fork: &EpisodeFork,
    session: &str,
    request: &ModelRequest,
) -> Result<ForkSize> {
    let unavailable = |reason: &str| models.fork_unavailable(session, &request.model, reason);
    let input_bytes = bubblepaw_core::prepared_request_bytes(request)?;
    if input_bytes > fork.max_request_bytes {
        return Err(unavailable("request_byte_limit"));
    }
    let tokenizer = tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
    let native_bytes = std::cell::Cell::new(0_usize);
    let estimate = fork
        .provider
        .estimate_summary_fork_tokens(request, &|value| {
            let serialized = value.to_string();
            native_bytes.set(native_bytes.get().max(serialized.len()));
            tokenizer
                .encode_ordinary(&serialized)
                .len()
                .saturating_add(8)
        });
    let tokens = match estimate {
        Ok(Some(tokens)) => tokens,
        _ => {
            return Err(unavailable("input_estimate_unavailable"));
        }
    };
    let input_bytes = input_bytes.max(native_bytes.get());
    if input_bytes > fork.max_request_bytes {
        return Err(unavailable("request_byte_limit"));
    }
    let physical_window = fork.provider.capabilities(&request.model).context_window;
    let window = match (fork.context_window, physical_window) {
        (Some(configured), Some(physical)) => Some(configured.min(physical)),
        (configured, physical) => configured.or(physical),
    };
    let output_reserve = request
        .max_output_tokens
        .map(|tokens| tokens as usize)
        .unwrap_or(0)
        .max(fork.output_reserve_tokens);
    if window.is_some_and(|window| tokens.saturating_add(output_reserve) > window) {
        return Err(unavailable("context_window_limit"));
    }
    Ok(ForkSize {
        input_bytes,
        tokens,
        output_reserve,
        window,
    })
}
