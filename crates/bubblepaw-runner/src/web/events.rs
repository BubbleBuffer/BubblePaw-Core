//! The live event stream. Missed events come from the host replay; live events
//! come from the page broadcast, which `start_tasks` feeds from the host.
use super::{HostEvent, WebState, host::host_ctx};
use crate::host_api::events;
use axum::{
    extract::State,
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use bubblepaw_core::HarnessEvent;
use futures_util::stream;
use std::{collections::VecDeque, convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::broadcast;

fn sse_event(event: &HarnessEvent) -> Event {
    Event::default()
        .id(event.sequence.to_string())
        .json_data(event)
        .unwrap()
}

/// The events after `last_id` that the client missed. None means the host
/// reported a resync: the log no longer holds them all, or there are too many.
async fn replayed_after(state: &WebState, last_id: Option<u64>) -> Option<(u64, VecDeque<Event>)> {
    let after = last_id?;
    let ctx = host_ctx(state);
    tokio::task::spawn_blocking(move || {
        let subscription = events::subscribe(&ctx, Some(after), None).ok()?;
        if subscription.resync {
            return None;
        }
        let cursor = subscription
            .replay
            .last()
            .map_or(after, |event| event.sequence);
        Some((cursor, subscription.replay.iter().map(sse_event).collect()))
    })
    .await
    .ok()
    .flatten()
}

/// Live events, after a `reset` or, with `Last-Event-ID`, after replaying the
/// logged events the client missed (docs/web-contract.md).
pub(super) async fn events_stream(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>> {
    let receiver = state.events.subscribe();
    let stop = state.stop.clone();
    let last_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let replayed = replayed_after(&state, last_id).await;
    // Before the first live event: a reset, or the replayed events.
    let (cursor, initial): (u64, VecDeque<Event>) = replayed.unwrap_or_else(|| {
        (
            0,
            VecDeque::from([Event::default().event("reset").data("{}")]),
        )
    });
    let stream = stream::unfold(
        (receiver, initial, cursor, stop),
        |(mut receiver, mut initial, mut cursor, stop)| async move {
            if let Some(event) = initial.pop_front() {
                return Some((Ok(event), (receiver, initial, cursor, stop)));
            }
            loop {
                let received = tokio::select! { _ = stop.cancelled() => return None, value = receiver.recv() => value };
                let event = match received {
                    Ok(HostEvent::Core(event)) => {
                        // Skip what the replay already sent.
                        if event.durable && event.sequence <= cursor
                            || !event.durable && event.sequence < cursor
                        {
                            continue;
                        }
                        cursor = cursor.max(event.sequence);
                        sse_event(event.as_ref())
                    }
                    Ok(HostEvent::Reset) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        Event::default().event("reset").data("{}")
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                };
                return Some((Ok(event), (receiver, initial, cursor, stop)));
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
