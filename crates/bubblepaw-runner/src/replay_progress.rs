//! Metadata-only progress file written while a replay runs.
use super::*;

/// Metadata-only progress, so a replay that times out still reports its counts.
/// report.json remains the only completed-run artifact; this is a diagnostic.
pub(super) const PROGRESS_INTERVAL_MESSAGES: usize = 100;

#[derive(Clone)]
pub(super) struct ProgressWriter {
    pub(super) out: PathBuf,
    pub(super) telemetry: Arc<Mutex<Telemetry>>,
    pub(super) started: Instant,
    pub(super) threshold: usize,
    pub(super) messages_total: usize,
    pub(super) archive_normalization: ArchiveNormalization,
    pub(super) store: Store,
    pub(super) phases: Arc<Mutex<ReplayPhaseTimings>>,
    pub(super) snapshots: Arc<Mutex<ReplayProgressSnapshot>>,
}

#[derive(Default)]
pub(super) struct ReplayPhaseTimings {
    pub(super) seconds: BTreeMap<&'static str, f64>,
    pub(super) current: Option<(&'static str, usize, Instant)>,
    pub(super) canonical_store_writes_seconds: f64,
}

pub(super) struct ReplayPhase {
    pub(super) phases: Arc<Mutex<ReplayPhaseTimings>>,
}

impl Drop for ReplayPhase {
    fn drop(&mut self) {
        let mut phases = self.phases.lock().unwrap();
        if let Some((name, _, began)) = phases.current.take() {
            *phases.seconds.entry(name).or_default() += began.elapsed().as_secs_f64();
        }
    }
}

#[derive(Default)]
pub(super) struct ReplayProgressSnapshot {
    pub(super) status: String,
    pub(super) messages_imported: usize,
    pub(super) compactions: usize,
    pub(super) import_transactions: u64,
}

pub(super) struct ReplayHeartbeat {
    pub(super) stop: std::sync::mpsc::Sender<()>,
    pub(super) thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ReplayHeartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl ProgressWriter {
    pub(super) fn phase(&self, name: &'static str, step: usize) -> ReplayPhase {
        let mut phases = self.phases.lock().unwrap();
        debug_assert!(phases.current.is_none(), "Replay phases must not nest");
        phases.current = Some((name, step, Instant::now()));
        ReplayPhase {
            phases: self.phases.clone(),
        }
    }

    pub(super) fn counts(&self, messages: usize, compactions: usize, transactions: u64) {
        let mut snapshot = self.snapshots.lock().unwrap();
        snapshot.messages_imported = messages;
        snapshot.compactions = compactions;
        snapshot.import_transactions = transactions;
    }

    pub(super) fn timings(&self) -> Value {
        let phases = self.phases.lock().unwrap();
        let mut seconds = phases.seconds.clone();
        let current = phases.current.map(|(name, step, began)| {
            let elapsed = began.elapsed().as_secs_f64();
            *seconds.entry(name).or_default() += elapsed;
            json!({"phase": name, "step": step, "elapsed_seconds": elapsed})
        });
        let importer_seconds = seconds
            .get("canonical_serialization")
            .copied()
            .unwrap_or(0.0)
            + seconds.get("canonical_commit").copied().unwrap_or(0.0);
        seconds.insert("canonical_import", importer_seconds);
        let store_seconds = self.store.write_seconds();
        let canonical_store_seconds = phases.canonical_store_writes_seconds;
        json!({
            "seconds": seconds,
            "current_operation": current,
            "store_writes_seconds": store_seconds,
            "canonical_store_writes_seconds": canonical_store_seconds,
            "internal_store_writes_seconds": (store_seconds - canonical_store_seconds).max(0.0),
            "basis": "Cumulative wall time, current operation included. canonical_import is canonical_serialization plus canonical_commit; worker_compaction includes planning, tokenization, model waits and internal Store writes. store_writes_seconds is the Store write-boundary counter, counted once, including canonical imports; canonical_store_writes_seconds records the counter delta around each profiled import, and internal_store_writes_seconds is the remainder. canonical_commit also includes importer overhead outside Store writes. Store timing excludes event journaling and schema initialization; uninstrumented time is not attributed."
        })
    }

    pub(super) fn heartbeat(&self) -> ReplayHeartbeat {
        let writer = self.clone();
        let (stop, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            while matches!(
                receiver.recv_timeout(Duration::from_secs(1)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                // Best-effort diagnostic; foreground publications still surface errors.
                let _ = writer.persist();
            }
        });
        ReplayHeartbeat {
            stop,
            thread: Some(thread),
        }
    }

    /// Writes to a temporary file and renames it, so a reader never sees a torn file.
    pub(super) fn write(
        &self,
        status: &str,
        messages_imported: usize,
        compactions: usize,
    ) -> Result<()> {
        {
            let mut snapshot = self.snapshots.lock().unwrap();
            snapshot.status = status.into();
            snapshot.messages_imported = messages_imported;
            snapshot.compactions = compactions;
        }
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        // Serialize foreground and heartbeat publications, including temp rename.
        let snapshot = self.snapshots.lock().unwrap();
        let timings = self.timings();
        let physical = physical_storage(&self.out.join("store"))?;
        let store_transactions = self
            .store
            .revision()
            .saturating_sub(snapshot.import_transactions);
        let progress = {
            let meter = self.telemetry.lock().unwrap();
            json!({
                "diagnostic": "Replay progress; not a completed report or common probe plan",
                "status": snapshot.status,
                "messages_imported": snapshot.messages_imported,
                "messages_total": self.messages_total,
                "source_messages_total": self.archive_normalization.source_messages_total,
                "source_messages_replayed": snapshot.messages_imported - self.archive_normalization.markers_replayed(snapshot.messages_imported),
                "archive_unknown_outcome_markers": self.archive_normalization.markers_replayed(snapshot.messages_imported),
                "archive_unknown_outcome_markers_total": self.archive_normalization.marker_steps.len(),
                "compactions_completed": snapshot.compactions,
                "phase_timings": timings,
                "storage": {
                    "database_bytes": physical["database_bytes"],
                    "chat_bytes": physical["chat_bytes"],
                    "serving_bytes": physical["serving_bytes"],
                    "catalog_bytes": physical["catalog_bytes"],
                    "wal_bytes": physical["wal_bytes"],
                    "store_transactions": store_transactions,
                    "canonical_import_transactions": snapshot.import_transactions,
                    "transaction_count": store_transactions + snapshot.import_transactions,
                    "basis": "Filesystem sizes and committed transaction counters; no SQL polling"
                },
                "resolved_threshold": self.threshold,
                "elapsed_seconds": self.started.elapsed().as_secs_f64(),
                "calls": meter.calls.len(),
                "failed_attempts": meter.failures,
                "projected_cost": {
                    "basis": "Scripted O200k estimates or provider-reported attempt usage; not a measured spend total",
                    "summary_and_chapter": projected_group(&meter.calls, &["summary", "chapter"]),
                    "decision": projected_group(&meter.calls, &["decision"]),
                },
            })
        };
        let temporary = self.out.join("progress.json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&progress)?)?;
        std::fs::rename(&temporary, self.out.join("progress.json"))?;
        Ok(())
    }
}
