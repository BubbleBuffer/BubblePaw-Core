//! The model bundle a replay runs against, and the scripted episode memory used when no model writes it.
use super::*;

pub(super) struct ReplayModels {
    pub(super) inner: Option<Arc<dyn EpisodeModels>>,
    pub(super) telemetry: Arc<Mutex<Telemetry>>,
    pub(super) store: Store,
    pub(super) estimator: Arc<SummarizingContext>,
    pub(super) episode_config: EpisodeConfig,
    pub(super) roles: crate::model_roles::ModelRoles,
    pub(super) prices: &'static PriceTable,
}
impl ReplayModels {
    pub(super) fn begin(&self, kind: &str) -> Result<()> {
        let mut meter = self.telemetry.lock().unwrap();
        meter.guard()?;
        meter.begin_operation(kind);
        Ok(())
    }
    pub(super) fn fake_call(&self, usage: &Usage) {
        let mut meter = self.telemetry.lock().unwrap();
        let kind = meter.current_kind.clone();
        let operation = meter.current_operation;
        let projected = if kind == "decision" {
            // OpenRouter tariffs come from live reported usage.cost only, so an
            // unpriced decider projects no scripted spend.
            let model = self.roles.decider.models.first().map_or("", String::as_str);
            self.prices.usage_cost(model, usage, None).unwrap_or(0.0)
        } else {
            let role = if kind == "chapter" {
                &self.roles.chapter
            } else {
                &self.roles.summary
            };
            self.prices
                .usage_cost(&role.models[0], usage, None)
                .expect("validated replay price")
        };
        meter.calls.push(json!({"kind":kind,"operation":operation,"physical_call_index":1,"request_slot":1,"attempt":1,"transport_retry":false,"outcome":"scripted","input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"reasoning_tokens":0,"cost_usd":0.0,"projected_paid_cost_usd":projected,"latency_ms":0}));
    }
    pub(super) fn record_kind(&self, session: &str) -> Result<()> {
        let mut snapshot = crate::history_snapshot::HistorySnapshot::index(&self.store, session)?;
        let coverage = self.store.episode_coverage_token(session)?;
        let end = coverage
            .as_ref()
            .map(|id| snapshot.position(id).map(|index| index + 1))
            .transpose()?
            .unwrap_or(0);
        if self
            .store
            .context_episodes(session)?
            .last()
            .is_some_and(|episode| Some(&episode.last_message_id) == coverage.as_ref())
        {
            snapshot.seed_complete_prefix(end);
        }
        snapshot.scan(end, &self.estimator)?;
        if let Some(planned) = snapshot
            .plan_with_config(end, &self.episode_config)?
            .episodes
            .first()
        {
            let kind = serde_json::to_value(planned.kind)?
                .as_str()
                .unwrap()
                .to_owned();
            self.telemetry
                .lock()
                .unwrap()
                .episode_kinds
                .insert(planned.message_range.first_message_id.clone(), kind);
        }
        Ok(())
    }
}
/// Scripted episode memory written as a summary would be: one prose line per
/// recorded message with its source ID and a short excerpt, never the
/// rendered transcript itself (that is invalid memory, as for a live model).
pub(super) fn scripted_episode_memory(episode_text: &str, cap: usize) -> String {
    static HEADER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let header = HEADER.get_or_init(|| {
        regex::Regex::new(r"(?m)^\[(\S+) (User|Assistant|Tool|System)\]$").unwrap()
    });
    let starts: Vec<_> = header.captures_iter(episode_text).collect();
    let mut lines = Vec::new();
    for (index, captures) in starts.iter().enumerate() {
        let whole = captures.get(0).unwrap();
        let end = starts
            .get(index + 1)
            .map_or(episode_text.len(), |next| next.get(0).unwrap().start());
        let body = &episode_text[whole.end()..end];
        let excerpt = body
            .lines()
            .map(str::trim)
            .find(|line| {
                !line.is_empty()
                    && !line.starts_with("[tool output ")
                    && !line.starts_with("Tool call ")
                    && !line.starts_with("[source ")
            })
            .and_then(|line| {
                // Keep the excerpt verbatim (the scripted decision reads the
                // `[new subject]` marker); defuse brackets only when needed.
                let words = line
                    .split_whitespace()
                    .take(24)
                    .collect::<Vec<_>>()
                    .join(" ");
                [words.clone(), words.replace(['[', ']', '`'], "'")]
                    .into_iter()
                    .find(|line| {
                        crate::episode_models::summary_output_reason("summary", line).is_none()
                    })
            })
            .unwrap_or_else(|| "(no plain text)".into());
        // The scripted decision reads the marker wherever the message has it.
        let excerpt = if body.contains("[new subject]") && !excerpt.contains("[new subject]") {
            format!("[new subject] {excerpt}")
        } else {
            excerpt
        };
        let role = match &captures[2] {
            "User" => "The user wrote",
            "Assistant" => "The agent wrote",
            "Tool" => "A tool returned",
            _ => "The host noted",
        };
        lines.push(format!("- {role} (source {}): {excerpt}", &captures[1]));
    }
    let memory = format!(
        "# Scripted replay episode\nThe recorded chat covered {} messages.\n{}\n\nOpen: Continue the recorded chat.",
        starts.len(),
        lines.join("\n")
    );
    let memory = crate::episodes::token_excerpt(&memory, cap);
    if crate::episode_models::summary_output_reason("summary", &memory).is_none() {
        memory
    } else {
        format!(
            "# Scripted replay episode\nThe recorded chat covered {} messages.\n\nOpen: Continue the recorded chat.",
            starts.len()
        )
    }
}
