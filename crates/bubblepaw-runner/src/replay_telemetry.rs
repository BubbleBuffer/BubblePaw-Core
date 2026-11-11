//! Per-run call, token and cost telemetry for a replay.
use super::*;

#[derive(Serialize)]
pub(super) struct Telemetry {
    pub(super) max_cost_usd: f64,
    pub(super) calls: Vec<Value>,
    pub(super) flash_cost_usd: f64,
    pub(super) jev_cost_usd: f64,
    pub(super) openai_cost_usd: f64,
    pub(super) haiku_cost_usd: f64,
    pub(super) reserved_unknown_cost_usd: f64,
    pub(super) failures: usize,
    pub(super) retries: usize,
    pub(super) consecutive_failures: usize,
    pub(super) stopped: Option<String>,
    pub(super) current_kind: String,
    pub(super) current_operation: usize,
    pub(super) physical_call_index: usize,
    pub(super) request_slot: usize,
    pub(super) http_attempt: usize,
    pub(super) previous_attempt_failed: bool,
    pub(super) episode_kinds: HashMap<String, String>,
}
impl Default for Telemetry {
    fn default() -> Self {
        Self {
            max_cost_usd: SPEND_LIMIT,
            calls: vec![],
            flash_cost_usd: 0.0,
            jev_cost_usd: 0.0,
            openai_cost_usd: 0.0,
            haiku_cost_usd: 0.0,
            reserved_unknown_cost_usd: 0.0,
            failures: 0,
            retries: 0,
            consecutive_failures: 0,
            stopped: None,
            current_kind: String::new(),
            current_operation: 0,
            physical_call_index: 0,
            request_slot: 0,
            http_attempt: 0,
            previous_attempt_failed: false,
            episode_kinds: HashMap::new(),
        }
    }
}

impl Telemetry {
    pub(super) fn begin_operation(&mut self, kind: &str) {
        self.current_kind = kind.into();
        self.current_operation += 1;
        self.physical_call_index = 0;
        self.request_slot = 0;
        self.http_attempt = 0;
        self.previous_attempt_failed = false;
    }

    /// A successful request followed by another call in the same operation is
    /// an intentional model repetition (Jev) or structural repair (summary).
    /// Only an immediately preceding failed HTTP/transport attempt makes the
    /// next physical call a transport retry. Operation boundaries reset this.
    pub(super) fn observe_attempt(&mut self, ok: bool) -> bool {
        self.physical_call_index += 1;
        let retry = self.previous_attempt_failed;
        if retry {
            self.http_attempt += 1;
            self.retries += 1;
        } else {
            self.request_slot += 1;
            self.http_attempt = 1;
        }
        self.previous_attempt_failed = !ok;
        if ok {
            self.consecutive_failures = 0;
        } else {
            self.failures += 1;
            self.consecutive_failures += 1;
        }
        retry
    }

    pub(super) fn guard(&self) -> Result<()> {
        ensure!(
            self.stopped.is_none(),
            "Replay stopped by spend/failure guard"
        );
        ensure!(
            self.flash_cost_usd
                + self.jev_cost_usd
                + self.openai_cost_usd
                + self.haiku_cost_usd
                + self.reserved_unknown_cost_usd
                < self.max_cost_usd,
            "Replay spend limit reached"
        );
        Ok(())
    }
    pub(super) fn reserve_next(&mut self, reservation: f64) -> bool {
        let allowed = self.guard().is_ok()
            && self.flash_cost_usd
                + self.jev_cost_usd
                + self.openai_cost_usd
                + self.haiku_cost_usd
                + self.reserved_unknown_cost_usd
                + reservation
                <= self.max_cost_usd;
        if !allowed && self.stopped.is_none() {
            self.stopped = Some(format!(
                "Spend guard: measured plus unknown spend and worst-case next response exceeds ${:.6}",
                self.max_cost_usd
            ));
        }
        allowed
    }
}
