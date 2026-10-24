//! Store calls made by web handlers. SQLite work runs on the blocking pool, so
//! the async runtime never waits on the database.
use super::*;

pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> std::result::Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map_err(Into::into)
}

/// Rejects an unknown session; the store read runs on the blocking pool.
pub(super) async fn known_session(
    state: &Arc<WebState>,
    id: &str,
) -> std::result::Result<(), ApiError> {
    let (state, id) = (state.clone(), id.to_owned());
    blocking(move || state.store.session(&id).map(drop)).await
}

/// The logged events after `last_id` that the client missed, without hidden
/// sessions. The replay and its hidden-session filter read SQLite on the blocking pool.
pub(super) async fn replayed_after(
    state: &WebState,
    last_id: Option<u64>,
) -> Option<(u64, VecDeque<Event>)> {
    let after = last_id?;
    let (harness, store) = (state.harness.clone(), state.store.clone());
    tokio::task::spawn_blocking(move || {
        replay(&harness, after).map(|events| {
            events.map(|events| {
                let cursor = events.last().map_or(after, |event| event.sequence);
                let visible = events.iter().filter(|event| {
                    store
                        .session(&event.session_id)
                        .is_ok_and(|session| !session.hidden)
                });
                (cursor, visible.map(sse_event).collect::<VecDeque<_>>())
            })
        })
    })
    .await
    .ok()
    .and_then(Result::ok)
    .flatten()
}
