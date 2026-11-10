//! Summary-fork support for the Messages wire: the source mapping and the
//! open-tool-loop rule shared with the request renderer.
use super::*;

/// New user tasks and completed assistant answers close earlier tool loops.
/// Generated host notices are observations within the current loop.
/// Separate reasoning-only records can precede the first call of an open loop.
pub(super) fn unfinished_tool_loop(messages: &[bubblepaw_core::Message]) -> Option<usize> {
    let mut start = None;
    let mut has_calls = false;
    for (index, message) in messages.iter().enumerate() {
        match message.role {
            Role::User if message.origin != Some(bubblepaw_core::MessageOrigin::Generated) => {
                start = None;
                has_calls = false;
            }
            Role::Assistant if !message.tool_calls.is_empty() => {
                start.get_or_insert(index);
                has_calls = true;
            }
            Role::Assistant if !message.content.is_empty() => {
                start = None;
                has_calls = false;
            }
            Role::Assistant => {
                start.get_or_insert(index);
            }
            _ => {}
        }
    }
    has_calls.then_some(start).flatten()
}

impl MessagesProvider {
    /// Source message ids mapped to the wire turn and content block ("turn.block")
    /// each one rendered into. Adjacent same-role messages share a turn, so one
    /// turn can list several sources; skipped records have no position.
    pub(super) fn fork_mapping(request: &ModelRequest) -> Result<Value> {
        let (_, blocks) = Self::build(request)?;
        let mut positions = vec![Vec::new(); request.messages.len()];
        for (turn, sources) in blocks.iter().enumerate() {
            for (block, &source) in sources.iter().enumerate() {
                positions[source].push(json!(format!("{turn}.{block}")));
            }
        }
        let sources: Vec<Value> = request.messages.iter().zip(positions).enumerate().map(|(index, (message, wire))| json!({
            "source_id":message.id,"prepared_position":index,"role":message.role,
            "origin":message.origin,"is_typed_user":bubblepaw_core::is_typed_user(message),
            "wire_positions":wire,
            "tool_call_ids":message.tool_calls.iter().map(|call| &call.id).collect::<Vec<_>>(),
            "tool_call_id":message.tool_call_id,
        })).collect();
        Ok(json!({
            "protocol":"anthropic-messages",
            "position_unit":"\"turn.block\": zero-based messages array index, then content block index (the system prompt is not in messages)",
            "sources":sources,
        }))
    }
}
