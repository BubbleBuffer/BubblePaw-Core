//! Side requests as forks of a parent lineage. A fork sends the parent's exact
//! prefix and appends only its own task; a fresh side call records why it is not
//! a fork, so its cache miss is declared rather than unexplained.
use super::*;

impl ContextState {
    /// Declared when a fresh side call runs on a different route than its parent.
    pub const SIDE_CALL_OTHER_MODEL: &str = "side_call_other_model";
    /// Declared when the route matches but reasoning or output settings differ.
    pub const SIDE_CALL_SETTINGS: &str = "side_call_settings";
    /// Declared when no confirmed parent request exists to fork.
    pub const SIDE_CALL_NO_CAPTURE: &str = "side_call_no_capture";
    /// Declared when the side call matches its parent but is not forked by design.
    pub const SIDE_CALL_UNFORKED: &str = "side_call_unforked";
    /// Declared for an explicitly configured episode-only input (summary_input=episode).
    pub const SIDE_CALL_EPISODE_INPUT: &str = "side_call_episode_input";

    /// Build a side request whose first `fork_point` messages are the parent
    /// lineage's prefix exactly as last sent (cut at or extended past its end).
    /// Messages after the fork point belong to the side call; `task`, when given,
    /// is appended as a tail reminder. The request keeps the parent's system
    /// prompt, tools and tool choice. It is classified against the parent, with
    /// the parent's cold split (`cold_message_indexes` applies only without a
    /// parent), and neither the parent's nor any other lineage's state changes.
    #[allow(clippy::too_many_arguments)]
    pub fn fork(
        &self,
        mut request: ModelRequest,
        session: &str,
        parent_kind: &str,
        fork_point: usize,
        request_kind: &str,
        cold_message_indexes: &[usize],
        task: Option<Reminder>,
    ) -> BuiltContext {
        let fork_point = fork_point.min(request.messages.len());
        if let Some(message) = task.and_then(|task| reminders::reminder_message(session, &[task])) {
            request.messages.push(message);
        }
        let len = request.messages.len();
        let mut state = self.state.lock().unwrap();
        let declaration = state.declaration(session);
        let parent = state
            .previous
            .get(&(session.to_owned(), parent_kind.to_owned()));
        // Measure with the parent's cold split, so the same bytes compare equal
        // however the caller annotated them, and the fork's cold breakpoint sits
        // where the parent's did.
        let indexes: BTreeSet<_> = match parent {
            Some(parent) => parent.cold_indexes.iter().copied().collect(),
            None => cold_message_indexes.iter().copied().collect(),
        };
        let full = measure(&request, &indexes, len);
        let prefix = measure(&request, &indexes, fork_point);
        let classification = match parent {
            None => ContextClassification::First,
            Some(before) if before.frozen != prefix.frozen => ContextClassification::FrozenChange,
            Some(before) if before.cold != prefix.cold => ContextClassification::ColdChange,
            Some(before) => {
                let common = before.history.len().min(prefix.hashes.len());
                let same = before
                    .history
                    .iter()
                    .zip(&prefix.hashes)
                    .take_while(|(a, b)| a == b)
                    .count();
                if same == common {
                    ContextClassification::Append
                } else {
                    ContextClassification::HistoryRewrite {
                        first_differing_index: same,
                    }
                }
            }
        };
        let changed = !matches!(
            classification,
            ContextClassification::First | ContextClassification::Append
        );
        let reason = declaration
            .filter(|(epoch, _)| {
                changed && parent.is_some_and(|prior| *epoch > prior.declaration_epoch)
            })
            .map(|(_, reason)| reason);
        // The parent's last request wrote its cache at its own end. Marking the
        // same block (or the fork point, when the fork cuts earlier) lets the fork
        // read it.
        let previous_len = parent
            .filter(|_| classification == ContextClassification::Append)
            .map(|before| (before.cold.message_count + before.history.len()).min(fork_point));
        let parent_history =
            parent.map_or(0, |before| before.history.len().min(prefix.hashes.len()));
        let mut record = ContextBuild {
            request_kind: request_kind.to_owned(),
            frozen: full.frozen.clone(),
            cold: full.cold.clone(),
            history: full.history.clone(),
            declared: reason.is_some(),
            declaration_reason: reason,
            invalid_cold_indexes: indexes.range(len..).count(),
            frozen_writer: None,
            seq: 0,
            since_previous_ms: None,
            classification,
        };
        // A fork reads the parent's cache: its prefix is the parent's unchanged part.
        let opening = log::Opening {
            session,
            lineage: request_kind,
            reads_from: parent_kind,
            model: &request.model,
            reasoning_effort: request.reasoning_effort,
            unchanged_bytes: log::unchanged_bytes(&record.classification, parent_history, &prefix),
            total_bytes: full.total(),
        };
        let superseded = state.log.open(opening, &mut record, |frozen, cold| {
            tier_contents(&request, &indexes, len, frozen, cold)
        });
        drop(state);
        self.write_log(superseded);
        BuiltContext {
            tiers: CacheTiers {
                cold_end: indexes.range(..len).next_back().map(|last| last + 1),
                previous_len,
            },
            record,
            request,
        }
    }

    /// Build a fresh side request as an ordinary lineage, declaring why it is not
    /// a fork of `parent_kind`. Without an explicit `reason` the parent's last
    /// request decides: another route, other generation settings, no parent yet,
    /// or a matching call that is unforked by design.
    pub fn build_side(
        &self,
        request: ModelRequest,
        session: &str,
        request_kind: &str,
        cold_message_indexes: &[usize],
        parent_kind: &str,
        reason: Option<&str>,
    ) -> BuiltContext {
        let reason = reason.map_or_else(
            || self.side_reason(&request, session, parent_kind),
            str::to_owned,
        );
        let mut built = self.build(request, session, request_kind, cold_message_indexes);
        if built.record.declaration_reason.is_none() {
            built.record.declared = true;
            let mut state = self.state.lock().unwrap();
            state.log.redeclare(session, request_kind, &reason);
            drop(state);
            built.record.declaration_reason = Some(reason);
        }
        built
    }

    /// Why a side call on `request` cannot share the parent's prefix.
    pub fn side_reason(&self, request: &ModelRequest, session: &str, parent_kind: &str) -> String {
        let state = self.state.lock().unwrap();
        match state
            .previous
            .get(&(session.to_owned(), parent_kind.to_owned()))
        {
            None => Self::SIDE_CALL_NO_CAPTURE,
            Some(parent) if parent.model != request.model => Self::SIDE_CALL_OTHER_MODEL,
            Some(parent) if parent.settings != settings(request) => Self::SIDE_CALL_SETTINGS,
            Some(_) => Self::SIDE_CALL_UNFORKED,
        }
        .to_owned()
    }
}
