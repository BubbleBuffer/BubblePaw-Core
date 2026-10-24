//! Append-only attempt ledger. An interrupted request has unknown spend and blocks reuse.
use anyhow::{Context, Result, ensure};
use bubblepaw_core::Usage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Scope {
    pub stage: String,
    pub arm: String,
    pub chat: String,
    pub stage_id: String,
    pub candidate: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Call {
    pub id: String,
    pub invocation: String,
    pub status: String,
    pub mode: String,
    #[serde(flatten)]
    pub scope: Scope,
    pub model: String,
    pub usage: Option<Usage>,
    pub cost_usd: Option<f64>,
    pub latency_ms: f64,
    pub error: Option<String>,
}
struct State {
    file: File,
    calls: BTreeMap<String, Call>,
    scope: Scope,
    sequence: usize,
    blocked: bool,
}
pub(super) struct Meter {
    state: Mutex<State>,
    pub invocation: String,
    cap: f64,
    dry: bool,
}
impl Meter {
    pub fn open(out: &Path, cap: f64, dry: bool) -> Result<Self> {
        let path = out.join("calls.jsonl");
        let mut calls = BTreeMap::new();
        if path.exists() {
            for line in std::fs::read_to_string(&path)?
                .lines()
                .filter(|s| !s.is_empty())
            {
                let call: Call = serde_json::from_str(line)
                    .context("Invalid call ledger; preserve it and use a new output directory")?;
                ensure!(
                    call.mode == if dry { "scripted" } else { "live" },
                    "Cannot mix live and scripted calls in one output directory"
                );
                calls.insert(call.id.clone(), call);
            }
        }
        let blocked = calls
            .values()
            .any(|c| c.status == "started" || c.cost_usd.is_none());
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            state: Mutex::new(State {
                file,
                calls,
                scope: Scope::default(),
                sequence: 0,
                blocked,
            }),
            invocation: SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_nanos()
                .to_string(),
            cap,
            dry,
        })
    }
    pub fn scope(&self, stage: &str, arm: &str, chat: &str, stage_id: &str, candidate: &str) {
        self.state.lock().unwrap().scope = Scope {
            stage: stage.into(),
            arm: arm.into(),
            chat: chat.into(),
            stage_id: stage_id.into(),
            candidate: candidate.into(),
        };
    }
    pub fn stage(&self, id: &str) {
        self.state.lock().unwrap().scope.stage_id = id.into();
    }
    pub fn candidate(&self, id: &str) {
        self.state.lock().unwrap().scope.candidate = id.into();
    }
    pub fn blocked(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.blocked || state.calls.values().filter_map(|c| c.cost_usd).sum::<f64>() > self.cap
    }
    pub fn begin(&self, model: &str) -> Result<Call> {
        let mut state = self.state.lock().unwrap();
        ensure!(
            !state.blocked,
            "Benchmark stopped: a previous attempt has unknown cost; reconcile its ledger before resuming"
        );
        ensure!(
            state.calls.values().filter_map(|c| c.cost_usd).sum::<f64>() <= self.cap,
            "Benchmark stopped: max-cost-usd exceeded"
        );
        state.sequence += 1;
        let call = Call {
            id: format!("{}-{}", self.invocation, state.sequence),
            invocation: self.invocation.clone(),
            status: "started".into(),
            mode: if self.dry { "scripted" } else { "live" }.into(),
            scope: state.scope.clone(),
            model: model.into(),
            usage: None,
            cost_usd: None,
            latency_ms: 0.0,
            error: None,
        };
        append(&mut state.file, &call)?;
        state.calls.insert(call.id.clone(), call.clone());
        Ok(call)
    }
    pub fn finish(
        &self,
        mut call: Call,
        usage: Option<Usage>,
        reported: Option<f64>,
        latency_ms: f64,
        failed: bool,
    ) -> Result<()> {
        call.status = "finished".into();
        call.latency_ms = latency_ms;
        call.cost_usd = if self.dry {
            Some(0.0)
        } else {
            reported.or_else(|| {
                usage.as_ref().and_then(|u| {
                    crate::pricing::PriceTable::shared().usage_cost(&call.model, u, None)
                })
            })
        };
        if !self.dry
            && reported.is_none()
            && call.model == "opencode-decisions/jev-1.13-free"
            && usage.is_some()
        {
            call.cost_usd = Some(0.0);
        }
        if call
            .cost_usd
            .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
        {
            call.cost_usd = None;
        }
        call.usage = usage;
        call.error = failed.then(|| "Provider request failed".into());
        let mut state = self.state.lock().unwrap();
        if call.cost_usd.is_none() {
            state.blocked = true;
        }
        append(&mut state.file, &call)?;
        state.calls.insert(call.id.clone(), call);
        Ok(())
    }
    pub fn totals(&self, predicate: impl Fn(&Call) -> bool) -> Value {
        let state = self.state.lock().unwrap();
        let calls: Vec<_> = state.calls.values().filter(|c| predicate(c)).collect();
        let mut usage = Usage::default();
        let mut latencies = Vec::new();
        let mut cost = 0.0;
        let mut unknown = 0;
        for call in &calls {
            if let Some(u) = &call.usage {
                usage.add(u);
            }
            if let Some(c) = call.cost_usd {
                cost += c;
            } else {
                unknown += 1;
            }
            if call.status == "finished" {
                latencies.push(call.latency_ms);
            }
        }
        latencies.sort_by(f64::total_cmp);
        let percentile = |p: f64| -> Option<f64> {
            (!latencies.is_empty()).then(|| {
                latencies[((latencies.len() as f64 * p).ceil() as usize).saturating_sub(1)]
            })
        };
        json!({"calls":calls.len(),"input_tokens":usage.input_tokens,"cached_tokens":usage.cache_read_tokens,
            "cache_creation_tokens":usage.cache_creation_tokens,"output_tokens":usage.output_tokens,
            "cost_usd":cost,"unknown_cost_calls":unknown,"p50_latency_ms":percentile(0.5),"p95_latency_ms":percentile(0.95),
            "errors":calls.iter().filter(|c|c.error.is_some() || c.status != "finished").count()})
    }
}
fn append(file: &mut File, call: &Call) -> Result<()> {
    serde_json::to_writer(&mut *file, call)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cost_cap_and_interrupted_attempt_survive_resume() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let meter = Meter::open(dir.path(), 0.1, false)?;
        let call = meter.begin("priced")?;
        meter.finish(call, Some(Usage::default()), Some(0.2), 1.0, false)?;
        assert!(
            meter
                .begin("priced")
                .unwrap_err()
                .to_string()
                .contains("max-cost-usd")
        );
        drop(meter);
        let meter = Meter::open(dir.path(), 0.3, false)?;
        meter.begin("interrupted")?;
        drop(meter);
        assert!(Meter::open(dir.path(), 1.0, false)?.begin("later").is_err());
        Ok(())
    }
}
