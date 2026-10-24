//! Delivery recovery trusts structured identities; legacy matching is migration-scoped.
use super::*;

pub(crate) struct DeliveryMarker {
    pub kind: &'static str,
    pub id: String,
    pub legacy: String,
}
impl DeliveryMarker {
    pub(crate) fn new(
        kind: &'static str,
        id: impl Into<String>,
        legacy: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            id: id.into(),
            legacy: legacy.into(),
        }
    }
}

pub(crate) fn record_delivers(
    store: &Store,
    owner: &str,
    record_id: &str,
    origin: Option<MessageOrigin>,
    text: &str,
    marker: &DeliveryMarker,
) -> Result<bool> {
    if origin != Some(MessageOrigin::Generated) {
        return Ok(false);
    }
    if let Some(metadata) = store.reminder_metadata(owner, record_id)? {
        if metadata
            .reminders
            .iter()
            .flat_map(|reminder| &reminder.deliveries)
            .any(|delivery| delivery.kind == marker.kind && delivery.id == marker.id)
        {
            return Ok(true);
        }
        // A new combined record may carry explicit provenance for a genuinely old
        // accepted input. Only that original source text gets legacy matching.
        for source in &metadata.legacy_sources {
            if source.text.starts_with("[host notice]") && source.text.contains(&marker.legacy) {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    Ok(store.reminder_legacy_eligible(owner, record_id)?
        && text.starts_with("[host notice]")
        && text.contains(&marker.legacy))
}

/// Scan bounded pages only when a caller has discovered known completion candidates.
pub(crate) fn undelivered_markers(
    store: &Store,
    owner: &str,
    markers: impl IntoIterator<Item = DeliveryMarker>,
) -> Result<HashSet<String>> {
    let markers = markers.into_iter().collect::<Vec<_>>();
    let mut pending = markers
        .iter()
        .map(|marker| marker.id.clone())
        .collect::<HashSet<_>>();
    for input in store.queued_inputs(owner)? {
        for marker in markers
            .iter()
            .filter(|marker| pending.contains(&marker.id))
            .collect::<Vec<_>>()
        {
            if record_delivers(store, owner, &input.id, input.origin, &input.text, marker)? {
                pending.remove(&marker.id);
            }
        }
    }
    let mut before = None;
    while !pending.is_empty() {
        let Some(page) = store.messages_before(owner, before.as_deref(), 128)? else {
            break;
        };
        if page.is_empty() {
            break;
        }
        for (_, message) in &page {
            for marker in markers
                .iter()
                .filter(|marker| pending.contains(&marker.id))
                .collect::<Vec<_>>()
            {
                if record_delivers(
                    store,
                    owner,
                    &message.id,
                    message.origin,
                    &message.content,
                    marker,
                )? {
                    pending.remove(&marker.id);
                }
            }
        }
        before = page.first().map(|(_, message)| message.id.clone());
    }
    Ok(pending)
}

#[cfg(test)]
#[path = "reminder_recovery_tests.rs"]
mod tests;
