//! Output validation keeps failure categories stable and bounds diagnostic text.
use super::*;

impl HttpEpisodeModels {
    pub(super) fn validate_summary_output(
        &self,
        kind: &str,
        session: &str,
        response: &ChatResult,
        text: &str,
    ) -> Result<()> {
        self.validate_summary_output_at(kind, session, response, text, 1)
    }
    pub(super) fn validate_summary_output_at(
        &self,
        kind: &str,
        session: &str,
        response: &ChatResult,
        text: &str,
        attempt: usize,
    ) -> Result<()> {
        let Some(reason) = summary_output_reason(kind, text) else {
            return Ok(());
        };
        Err(self.output_failure(
            kind,
            session,
            &response.model,
            (reason, response.reasoning.as_deref()),
            attempt,
            response.usage.clone(),
        ))
    }
    pub(super) fn output_failure(
        &self,
        kind: &str,
        session: &str,
        model: &str,
        failure: (&str, Option<&str>),
        attempt: usize,
        usage: Usage,
    ) -> anyhow::Error {
        let (reason, reasoning) = failure;
        if let Some(trace) = &self.trace {
            trace.runtime_info(json!({
                "event":"episode_model_output_failed", "kind":kind,
                "session_id":session, "model":model,
                "reason":reason, "attempt":attempt, "usage":usage,
            }));
        }
        let mut detail = format!("{kind}: {reason}");
        if reason == "empty_output"
            && let Some(reasoning) = reasoning.filter(|text| !text.is_empty())
        {
            let preview: String = reasoning.chars().take(500).collect();
            detail.push_str(&format!("; reasoning preview: {preview}"));
        }
        with_episode_usage(anyhow!(InvalidMemoryOutput(detail)), usage)
    }
}
