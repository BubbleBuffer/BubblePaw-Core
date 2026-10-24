//! Disposable, immutable main-input snapshots for branch-local episode memory.
use super::SummarizingContext;
use crate::{episode_models::EpisodeFork, history_snapshot::HistorySnapshot};
use anyhow::Result;
use bubblepaw_core::{Message, ModelRequest, ModelResponse, Provider};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum CapturePhase {
    Candidate,
    Pending,
    Completed,
}

#[derive(Clone)]
pub(super) struct CapturedRequest {
    request: ModelRequest,
    cold_messages: Vec<Message>,
    provider: Arc<dyn Provider>,
    phase: CapturePhase,
    frontier: Option<i64>,
    /// While a main call is pending its fallback route is not yet confirmed.
    /// Keep the last completed prefix usable until the new response promotes it.
    previous: Option<Arc<CapturedRequest>>,
}

impl SummarizingContext {
    /// Confirmed main route only; an in-flight request cannot replace it with
    /// an alias whose successful fallback route is not known yet.
    pub fn episode_fork_route(&self, session: &str) -> Option<String> {
        let captures = self.captures.lock().unwrap();
        let saved = captures.get(session)?;
        match saved.phase {
            CapturePhase::Completed => Some(saved.request.model.clone()),
            CapturePhase::Pending => saved
                .previous
                .as_ref()
                .map(|previous| previous.request.model.clone()),
            CapturePhase::Candidate => None,
        }
    }

    pub(super) fn capture_episode_request(
        &self,
        session: &str,
        request: &ModelRequest,
        provider: Arc<dyn Provider>,
        dispatched: bool,
    ) {
        let frontier = self
            .store
            .context_view(session)
            .ok()
            .flatten()
            .and_then(|view| view.frontier);
        let mut captures = self.captures.lock().unwrap();
        // A preparation candidate must never overwrite a naturally warmed input.
        if !dispatched
            && captures
                .get(session)
                .is_some_and(|saved| saved.phase != CapturePhase::Candidate)
        {
            return;
        }
        let previous = if dispatched {
            captures.get(session).and_then(|saved| match saved.phase {
                CapturePhase::Completed => Some(saved.clone()),
                CapturePhase::Pending => saved.previous.clone(),
                CapturePhase::Candidate => None,
            })
        } else {
            None
        };
        captures.insert(
            session.into(),
            Arc::new(CapturedRequest {
                cold_messages: self
                    .saved_view_cold_indexes(session, request)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|index| request.messages.get(index).cloned())
                    .collect(),
                request: request.clone(),
                provider,
                phase: if dispatched {
                    CapturePhase::Pending
                } else {
                    CapturePhase::Candidate
                },
                frontier,
                previous,
            }),
        );
    }

    pub(super) fn observe_episode_route(&self, session: &str, response: &ModelResponse) {
        let selected = response
            .provider_state
            .as_ref()
            .and_then(|state| state["_bubblepaw_selected_model"].as_str());
        let mut captures = self.captures.lock().unwrap();
        if let Some(saved) = captures.get_mut(session) {
            // Existing snapshots remain immutable. In the common case no reader
            // owns this pending Arc, so promotion does not copy its prompt.
            let saved = Arc::make_mut(saved);
            if let Some(selected) = selected {
                saved.request.model = selected.into();
            }
            if saved.phase == CapturePhase::Pending {
                saved.phase = CapturePhase::Completed;
                saved.previous = None;
            }
        }
    }

    pub(crate) fn forget_episode_capture(&self, session: &str) {
        self.captures.lock().unwrap().remove(session);
        self.meters.lock().unwrap().remove(session);
    }

    pub(super) fn release_episode_capture(&self, session: &str) {
        let live = self
            .store
            .active_session_metadata(session)
            .is_ok_and(|value| value.is_some());
        let worker = self.worker.lock().unwrap().upgrade();
        if live
            && self.automatic
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
            && let Some(worker) = worker
        {
            // Hidden children release their provider as soon as their run ends.
            // Let the same background worker consume that final warm prefix
            // before discarding it, without holding up the completed main turn.
            let marker = self.captures.lock().unwrap().get(session).cloned();
            worker.trigger(session, 0, 0);
            let context = self.clone();
            let session = session.to_owned();
            runtime.spawn(async move {
                worker.wait_idle(&session).await;
                let mut captures = context.captures.lock().unwrap();
                if captures
                    .get(&session)
                    .zip(marker.as_ref())
                    .is_some_and(|(current, old)| Arc::ptr_eq(current, old))
                {
                    captures.remove(&session);
                }
            });
            return;
        }
        self.forget_episode_capture(session);
    }

    pub(crate) fn reconcile_episode_captures(&self, live: &[String]) {
        self.captures
            .lock()
            .unwrap()
            .retain(|id, _| live.contains(id));
        self.meters
            .lock()
            .unwrap()
            .retain(|id, _| live.contains(id));
    }

    pub(crate) fn clear_episode_captures(&self) {
        self.captures.lock().unwrap().clear();
        self.meters.lock().unwrap().clear();
    }

    /// Freeze an exact canonical source range over the latest main prefix.
    /// None defers strict fork gathering: no snapshot, missing or projected raw
    /// source, or a tool group crossing the requested frontier. It never selects
    /// a different summary-input policy.
    pub fn episode_fork(
        &self,
        session: &str,
        first: &str,
        last: &str,
    ) -> Result<Option<EpisodeFork>> {
        Ok(self
            .episode_fork_with_context(session, first, last)?
            .map(|(fork, _)| fork))
    }

    pub(crate) fn episode_fork_with_context(
        &self,
        session: &str,
        first: &str,
        last: &str,
    ) -> Result<Option<(EpisodeFork, Vec<usize>)>> {
        let Some(saved) = self.captures.lock().unwrap().get(session).cloned() else {
            return Ok(None);
        };
        let saved = if saved.phase == CapturePhase::Pending {
            let Some(previous) = saved.previous.clone() else {
                return Ok(None);
            };
            previous
        } else {
            saved
        };
        let snapshot = HistorySnapshot::index(&self.store, session)?;
        let first_index = snapshot.position(first)?;
        let last_index = snapshot.position(last)?;
        if first_index > last_index
            || saved
                .frontier
                .is_some_and(|frontier| frontier >= snapshot.seq(first_index))
        {
            return Ok(None);
        }
        let positions: HashMap<_, _> = snapshot
            .metadata
            .iter()
            .enumerate()
            .map(|(index, message)| (message.id.as_str(), index))
            .collect();
        let mut request = saved.request.clone();
        // Stop at the first future canonical message. In particular, task
        // anchors brought forward by serving preparation cannot leak the future.
        let future = request
            .messages
            .iter()
            .position(|message| {
                positions
                    .get(message.id.as_str())
                    .is_some_and(|index| *index > last_index)
            })
            .unwrap_or(request.messages.len());
        let end = request
            .messages
            .iter()
            .position(|message| message.id == last)
            .map_or(request.messages.len(), |index| index + 1);
        let stop = future.min(end);
        request.messages.truncate(stop);
        let Some(captured_end) = request
            .messages
            .iter()
            .filter_map(|message| positions.get(message.id.as_str()).copied())
            .max()
        else {
            return Ok(None);
        };
        // A synthetic compacted prefix cannot substitute for missing source.
        let mut source = HashMap::new();
        let mut previous = None;
        for message in &request.messages {
            if let Some(&index) = positions.get(message.id.as_str())
                && index >= first_index
            {
                if previous.is_some_and(|previous| previous >= index) {
                    return Ok(None);
                }
                previous = Some(index);
                if source.insert(index, message).is_some() {
                    return Ok(None);
                }
            }
        }
        let mut exact = true;
        snapshot.walk(
            first_index,
            (captured_end + 1).min(last_index + 1),
            |index, message| {
                exact &= source.get(&index).is_some_and(|saved| **saved == message);
                Ok(())
            },
        )?;
        if !exact {
            return Ok(None);
        }
        // Append every completed canonical row after the observed prefix,
        // including source preceding FIRST when the snapshot is older.
        snapshot.walk(captured_end + 1, last_index + 1, |_, message| {
            request.messages.push(message);
            Ok(())
        })?;
        if !complete_tool_groups(&request.messages) {
            return Ok(None);
        }
        let config = self.config_for(session, &request.model);
        let cold = request
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| saved.cold_messages.contains(message))
            .map(|(index, _)| index)
            .collect();
        Ok(Some((
            EpisodeFork {
                request,
                first_message_id: first.into(),
                last_message_id: last.into(),
                provider: saved.provider.clone(),
                max_request_bytes: config.max_request_bytes,
                context_window: config.context_window,
                output_reserve_tokens: config.lcm.output_reserve_tokens,
                dispatched: saved.phase == CapturePhase::Completed,
            },
            cold,
        )))
    }
    /// The latest confirmed main capture, cut at or extended with completed
    /// canonical rows to LAST, for a chapter worker fork. Unlike a summary fork
    /// it selects no source range: the chapter input travels in the fork's tail,
    /// so the prefix only has to be what the main loop sent. None when there is
    /// no confirmed capture or the cut would split a tool group.
    pub(crate) fn chapter_fork(
        &self,
        session: &str,
        last: &str,
    ) -> Result<Option<(EpisodeFork, Vec<usize>)>> {
        let saved = match self.captures.lock().unwrap().get(session) {
            Some(saved) if saved.phase == CapturePhase::Completed => saved.clone(),
            Some(saved) if saved.phase == CapturePhase::Pending => match &saved.previous {
                Some(previous) => previous.clone(),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        let snapshot = HistorySnapshot::index(&self.store, session)?;
        let last_index = snapshot.position(last)?;
        let positions: HashMap<_, _> = snapshot
            .metadata
            .iter()
            .enumerate()
            .map(|(index, message)| (message.id.as_str(), index))
            .collect();
        let mut request = saved.request.clone();
        let future = request
            .messages
            .iter()
            .position(|message| {
                positions
                    .get(message.id.as_str())
                    .is_some_and(|index| *index > last_index)
            })
            .unwrap_or(request.messages.len());
        request.messages.truncate(future);
        let Some(captured_end) = request
            .messages
            .iter()
            .filter_map(|message| positions.get(message.id.as_str()).copied())
            .max()
        else {
            return Ok(None);
        };
        snapshot.walk(captured_end + 1, last_index + 1, |_, message| {
            request.messages.push(message);
            Ok(())
        })?;
        if !complete_tool_groups(&request.messages) {
            return Ok(None);
        }
        let config = self.config_for(session, &request.model);
        let cold = request
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| saved.cold_messages.contains(message))
            .map(|(index, _)| index)
            .collect();
        Ok(Some((
            EpisodeFork {
                request,
                first_message_id: last.into(),
                last_message_id: last.into(),
                provider: saved.provider.clone(),
                max_request_bytes: config.max_request_bytes,
                context_window: config.context_window,
                output_reserve_tokens: config.lcm.output_reserve_tokens,
                dispatched: true,
            },
            cold,
        )))
    }
}

fn complete_tool_groups(messages: &[Message]) -> bool {
    let mut pending = HashSet::new();
    for message in messages {
        if let Some(call) = &message.tool_call_id
            && !pending.remove(call.as_str())
        {
            return false;
        }
        for call in &message.tool_calls {
            if !pending.insert(call.id.as_str()) {
                return false;
            }
        }
        // A new conversation turn while a prior group is unfinished is unsafe.
        if message.tool_call_id.is_none() && message.tool_calls.is_empty() && !pending.is_empty() {
            return false;
        }
    }
    pending.is_empty()
}
