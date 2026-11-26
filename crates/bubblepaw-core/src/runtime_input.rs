//! Accepting steering and follow-up input while a session runs.
use super::*;
impl Harness {
    pub fn steer(&self, session_id: &str, text: impl Into<MessageInput>) -> Result<QueuedInput> {
        self.steer_from(session_id, text, None)
    }
    pub fn followup(&self, session_id: &str, text: impl Into<MessageInput>) -> Result<QueuedInput> {
        self.followup_from(session_id, text, None)
    }
    /// Steer, recording the client on SteeringAccepted.
    pub fn steer_from(
        &self,
        session_id: &str,
        text: impl Into<MessageInput>,
        client: Option<ClientInfo>,
    ) -> Result<QueuedInput> {
        self.accept(session_id, InputKind::Steer, text.into(), client)
    }
    /// Queue a followup, recording the client on FollowupQueued.
    pub fn followup_from(
        &self,
        session_id: &str,
        text: impl Into<MessageInput>,
        client: Option<ClientInfo>,
    ) -> Result<QueuedInput> {
        self.accept(session_id, InputKind::Followup, text.into(), client)
    }
    fn accept(
        &self,
        session_id: &str,
        kind: InputKind,
        input: MessageInput,
        client: Option<ClientInfo>,
    ) -> Result<QueuedInput> {
        let active = self.state.admission_guard(session_id);
        ensure!(
            !self.state.stopping.load(Ordering::Relaxed),
            "Harness is shutting down"
        );
        let text = input.text().into_owned();
        ensure!(
            text.len() <= self.config.max_request_bytes,
            "Input exceeds request size limit"
        );
        let content = input.into_blocks();
        let run_id = active
            .get(session_id)
            .filter(|a| a.finished.is_none())
            .map(|a| a.run_id.clone());
        self.store.session(session_id)?;
        let input = QueuedInput {
            id: id(),
            session_id: session_id.into(),
            kind: kind.clone(),
            content,
            state: "pending".into(),
            origin: Some(crate::MessageOrigin::Typed),
        };
        self.journals
            .session
            .input_queued(&input, run_id.as_deref(), client)?;
        // Admission only queues input. Releasing foreground waits is an
        // explicit host action, independent of accepting ordinary steering.
        if let Some(observer) = &self.config.observation {
            observer.emit(ObservationScope { session_id:session_id.into(),run_id:run_id.clone().unwrap_or_default(),..Default::default() }, "input_accepted", serde_json::json!({"input_id":input.id,"input_kind":kind,"text_bytes":text.len(),"fingerprint":crate::fingerprint(text.as_bytes())}), Some(serde_json::json!({"text":text})));
        }
        Ok(input)
    }
}
