//! Race-free subscription: subscribe first, then replay the durable log.
use super::*;
use bubblepaw_core::HarnessEvent;
use tokio::sync::broadcast;

pub const REPLAY_LIMIT: usize = 10 * bubblepaw_core::EVENTS_AFTER_LIMIT;
pub struct EventSubscription {
    pub receiver: broadcast::Receiver<HarnessEvent>,
    pub replay: Vec<HarnessEvent>,
    pub resync: bool,
    /// Replay boundary; discard live durable events at or below this sequence.
    pub last_seq: u64,
}

/// Hidden sessions are included only in an explicitly session-scoped stream.
pub fn visible(ctx: &HostCtx, event: &HarnessEvent, session_id: Option<&str>) -> bool {
    ctx.scope.contains(&ctx.store, &event.session_id)
        && ctx
            .read_scope
            .allows(&ctx.store, &event.session_id)
            .unwrap_or(false)
        && session_id.is_none_or(|id| id == event.session_id)
        && (session_id.is_some()
            || ctx
                .store
                .session(&event.session_id)
                .is_ok_and(|s| !s.hidden))
}

pub fn subscribe(
    ctx: &HostCtx,
    after: Option<u64>,
    session_id: Option<&str>,
) -> Result<EventSubscription, HostError> {
    if let Some(id) = session_id {
        ctx.ensure_read_session(id)?;
    }
    let receiver = ctx.harness.subscribe();
    let mut last_seq = ctx.harness.last_sequence();
    let mut replay = Vec::new();
    let mut resync = false;
    if let Some(mut cursor) = after {
        if !ctx.harness.can_resume_from(cursor) {
            resync = true;
        } else {
            last_seq = last_seq.max(cursor);
            let mut visited = 0usize;
            loop {
                let page = ctx.harness.events_after(
                    cursor,
                    session_id,
                    bubblepaw_core::EVENTS_AFTER_LIMIT,
                )?;
                if page.is_empty() {
                    break;
                }
                visited = visited.saturating_add(page.len());
                if visited > REPLAY_LIMIT {
                    replay.clear();
                    resync = true;
                    break;
                }
                let through = page.last().map_or(cursor, |event| event.sequence);
                replay.extend(
                    page.into_iter().filter(|event| {
                        event.sequence <= last_seq && visible(ctx, event, session_id)
                    }),
                );
                if through >= last_seq || through <= cursor {
                    break;
                }
                cursor = through;
            }
        }
    }
    Ok(EventSubscription {
        receiver,
        replay,
        resync,
        last_seq,
    })
}

#[derive(Serialize)]
pub struct EventsPage {
    pub events: Vec<HarnessEvent>,
    pub last_seq: u64,
    pub resync: bool,
}
pub(super) fn after(ctx: &HostCtx, params: Value) -> Result<Value, HostError> {
    let input: EventsParams = decode(params)?;
    if let Some(id) = &input.session_id {
        ctx.ensure_read_session(id)?;
    }
    let after = input.after.unwrap_or(0);
    let events = ctx
        .harness
        .events_after(
            after,
            input.session_id.as_deref(),
            input.limit.unwrap_or(bubblepaw_core::EVENTS_AFTER_LIMIT),
        )?
        .into_iter()
        .filter(|e| visible(ctx, e, input.session_id.as_deref()))
        .collect();
    encode(EventsPage {
        events,
        last_seq: ctx.harness.last_sequence(),
        resync: !ctx.harness.can_resume_from(after),
    })
}
