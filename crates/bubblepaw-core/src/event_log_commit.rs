//! Commit one canonical boundary and its ordered session event projections.
use super::*;

pub(crate) struct PendingEvent {
    pub session_id: String,
    pub run_id: String,
    pub kind: EventKind,
    pub reference: Option<ContentRef>,
}
impl PendingEvent {
    pub(crate) fn new(session: &str, run: &str, kind: EventKind) -> Self {
        Self {
            session_id: session.into(),
            run_id: run.into(),
            kind,
            reference: None,
        }
    }
}
fn source_events(session: &str, run: &str, events: Vec<JournalEvent>) -> Vec<PendingEvent> {
    events
        .into_iter()
        .map(|(kind, reference)| PendingEvent {
            reference,
            ..PendingEvent::new(session, run, kind)
        })
        .collect()
}

impl Log {
    /// Canonical state and every targeted event are accepted under one snapshot lock.
    pub(crate) fn commit<T>(
        &self,
        store: &Store,
        session_id: &str,
        run_id: &str,
        origin: Option<ClientInfo>,
        write: impl FnOnce(&Connection) -> Result<JournalBatch<T>>,
    ) -> Result<T> {
        self.commit_impl(store, session_id, origin, false, false, |tx| {
            let (value, events) = write(tx)?;
            Ok((value, source_events(session_id, run_id, events)))
        })
    }
    pub(crate) fn commit_reply<T>(
        &self,
        store: &Store,
        session_id: &str,
        run_id: &str,
        origin: Option<ClientInfo>,
        write: impl FnOnce(&Connection) -> Result<JournalBatch<T>>,
    ) -> Result<T> {
        self.commit_impl(store, session_id, origin, true, false, |tx| {
            let (value, events) = write(tx)?;
            Ok((value, source_events(session_id, run_id, events)))
        })
    }
    /// Gather retains its existing telemetry split and canonical rollback boundary.
    pub(crate) fn commit_context<T>(
        &self,
        store: &Store,
        session_id: &str,
        write: impl FnOnce(&Connection) -> Result<JournalBatch<T>>,
    ) -> Result<T> {
        self.commit_impl(store, session_id, None, false, true, |tx| {
            let (value, events) = write(tx)?;
            Ok((value, source_events(session_id, "", events)))
        })
    }
    pub(crate) fn commit_targeted<T>(
        &self,
        store: &Store,
        session_id: &str,
        write: impl FnOnce(&Connection) -> Result<(T, Vec<PendingEvent>)>,
    ) -> Result<T> {
        self.commit_impl(store, session_id, None, false, false, write)
    }
    #[allow(clippy::too_many_arguments)]
    fn commit_impl<T>(
        &self,
        store: &Store,
        session_id: &str,
        origin: Option<ClientInfo>,
        settle_reply: bool,
        context: bool,
        write: impl FnOnce(&Connection) -> Result<(T, Vec<PendingEvent>)>,
    ) -> Result<T> {
        // Opening a chat can recover unfinished runs and publish their events.
        // Resolve it before taking the shared journal lock.
        let chat = store.scoped(session_id)?.unwrap_or_else(|| store.clone());
        let mut state = self.state.lock().unwrap();
        state.pruned_through = state
            .pruned_through
            .max(self.pruned_floor.load(Ordering::Relaxed));
        let write_record = |tx: &Connection| {
            let (value, mut kinds) = write(tx)?;
            kinds.extend(parent_run_events(tx, &kinds)?);
            let reserved_through = state
                .last
                .checked_add(kinds.len() as u64)
                .context("Event sequence overflow")?;
            store.ensure_event_sequences(reserved_through)?;
            let mut events = Vec::with_capacity(kinds.len());
            for (offset, pending) in kinds.into_iter().enumerate() {
                let PendingEvent {
                    session_id,
                    run_id,
                    kind,
                    reference,
                } = pending;
                let event = HarnessEvent {
                    sequence: state.last + offset as u64 + 1,
                    durable: true,
                    origin: if matches!(&kind, EventKind::SessionSettled {}) {
                        None
                    } else {
                        origin.clone()
                    },
                    session_id,
                    run_id,
                    created_at: now(),
                    kind,
                };
                let encoded = encode(tx, &event, reference)?;
                if !store.is_layout() {
                    append_encoded(tx, &event, &encoded)?;
                }
                events.push((event, encoded));
            }
            Ok((value, events))
        };
        let (value, mut events) = if context {
            chat.write_context(write_record)
        } else {
            chat.write(write_record)
        }?;
        if store.is_layout() {
            // The authoritative record is already committed. Journal failure
            // requires clients to resync; it must never trigger effect replay.
            let journal = if store.take_projection_gap() {
                Err(anyhow::anyhow!("Disposable metadata projection failed"))
            } else {
                store.write(|tx| {
                    remember_last(
                        tx,
                        events
                            .last()
                            .map_or(state.last, |(event, _)| event.sequence),
                    )?;
                    if state.pruned_through > 0 {
                        tx.execute(
                            "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                            params![PRUNED_KEY, state.pruned_through.to_string()],
                        )?;
                    }
                    for (event, encoded) in &events {
                        append_encoded(tx, event, encoded)?;
                    }
                    Ok(())
                })
            };
            if journal.is_err() {
                if let Some((event, _)) = events.last() {
                    state.last = event.sequence;
                    state.pruned_through = state.last;
                }
                for (event, _) in &mut events {
                    event.durable = false;
                }
                // A rejected event INSERT need not block the small gap marker.
                // Preserve resync across reopen without storing journal content
                // in the authoritative chat or turning record success into error.
                let _ = store.write(|tx| {
                    remember_last(tx, state.last)?;
                    tx.execute(
                        "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                        params![PRUNED_KEY, state.pruned_through.to_string()],
                    )?;
                    Ok(())
                });
            }
        }
        if settle_reply && let Some(streaming) = state.streaming.get_mut(session_id) {
            streaming.text.clear();
        }
        for (event, _) in events {
            state.last = event.sequence;
            state.live.clear();
            match &event.kind {
                EventKind::RunStarted { .. } => {
                    state.streaming.insert(
                        event.session_id.clone(),
                        Streaming {
                            run_id: event.run_id.clone(),
                            text: String::new(),
                        },
                    );
                }
                EventKind::RunFinished { .. } | EventKind::SessionSettled {} => {
                    state.streaming.remove(&event.session_id);
                }
                _ => {}
            }
            if let Some(sender) = self.sender.get() {
                let _ = sender.send(event);
            }
        }
        Ok(value)
    }
}

fn parent_run_events(tx: &Connection, events: &[PendingEvent]) -> Result<Vec<PendingEvent>> {
    let mut projected = Vec::new();
    for event in events {
        if !matches!(
            event.kind,
            EventKind::RunStarted { .. } | EventKind::RunFinished { .. }
        ) {
            continue;
        }
        // Only a real stored run boundary is projected; a public diagnostic
        // emitter without an owning run row cannot invent a session state.
        let state: Option<String> = tx
            .query_row(
                "SELECT status FROM runs WHERE session_id=?1 AND id=?2",
                params![event.session_id, event.run_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(state) = state
            && let Some(parent) =
                parent_run_state_event(tx, &event.session_id, &event.run_id, &state)?
        {
            projected.push(parent);
        }
    }
    Ok(projected)
}

pub(crate) fn parent_run_state_event(
    tx: &Connection,
    session: &str,
    run: &str,
    state: &str,
) -> Result<Option<PendingEvent>> {
    let data: Option<String> = tx
        .query_row("SELECT data FROM sessions WHERE id=?1", [session], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(data) = data else {
        return Ok(None);
    };
    let session: crate::Session = serde_json::from_str(&data)?;
    let Some(parent) = session.parent_session_id.filter(|_| !session.hidden) else {
        return Ok(None);
    };
    Ok(Some(PendingEvent::new(
        &parent,
        run,
        EventKind::ChildRunStateChanged {
            child_session_id: session.id,
            state: state.into(),
        },
    )))
}
