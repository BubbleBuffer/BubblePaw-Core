//! Opt-in local JSONL observations. This adapter never opens the canonical archive.
//! A path must have a single writer; use a separate path for concurrent runners.
use anyhow::{Context, Result, anyhow, ensure};
use bubblepaw_core::{
    Observation, ObservationMode, ObservationRecord, ObservationScope, ObservationSink,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct TraceConfig {
    pub path: PathBuf,
    pub mode: ObservationMode,
    pub label: String,
    pub queue_capacity: usize,
    pub max_record_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_file_bytes: u64,
    /// Total files including the current file. Rotation names are PATH.1, PATH.2, ...
    pub retained_files: usize,
}
impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            path: "trace.jsonl".into(),
            mode: ObservationMode::Metadata,
            label: String::new(),
            queue_capacity: 256,
            max_record_bytes: 64 * 1024,
            max_payload_bytes: 32 * 1024,
            max_file_bytes: 32 * 1024 * 1024,
            retained_files: 3,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TraceStatus {
    pub accepted: u64,
    pub written: u64,
    pub dropped: u64,
    pub write_errors: u64,
    pub rotations: u64,
    pub retention_evictions: u64,
    pub last_error: Option<String>,
}
#[derive(Default)]
struct Counters {
    diagnostics: crate::cli_output::Output,
    accepted: AtomicU64,
    written: AtomicU64,
    dropped: AtomicU64,
    write_errors: AtomicU64,
    rotations: AtomicU64,
    retention_evictions: AtomicU64,
    last_error: Mutex<Option<String>>,
}
impl Counters {
    fn snapshot(&self) -> TraceStatus {
        TraceStatus {
            accepted: self.accepted.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
            rotations: self.rotations.load(Ordering::Relaxed),
            retention_evictions: self.retention_evictions.load(Ordering::Relaxed),
            last_error: self.last_error.lock().unwrap().clone(),
        }
    }
    fn error(&self, error: &dyn std::fmt::Display) {
        self.write_errors.fetch_add(1, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(error.to_string());
    }
}
enum QueueItem {
    Record(Box<ObservationRecord>),
    Flush(SyncSender<()>),
}
struct Inner {
    config: TraceConfig,
    sender: Mutex<Option<SyncSender<QueueItem>>>,
    counters: Arc<Counters>,
    done: Mutex<Receiver<()>>,
    writer: Mutex<Option<JoinHandle<()>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.sender.get_mut().unwrap().take();
        if self.writer.get_mut().unwrap().is_none() {
            return;
        }
        // A stuck filesystem must never hang shutdown. Detached writer owns its buffer.
        if self
            .done
            .get_mut()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .is_ok()
            && let Some(writer) = self.writer.get_mut().unwrap().take()
        {
            let _ = writer.join();
        }
    }
}
#[derive(Clone)]
pub struct JsonlTrace(Arc<Inner>);
impl JsonlTrace {
    pub fn open(config: TraceConfig) -> Result<Self> {
        Self::open_with_output(config, crate::cli_output::Output::default())
    }

    pub fn open_with_output(
        config: TraceConfig,
        diagnostics: crate::cli_output::Output,
    ) -> Result<Self> {
        ensure!(
            config.queue_capacity > 0 && config.queue_capacity <= 65536,
            "Trace queue capacity must be 1..65536"
        );
        ensure!(
            config.max_record_bytes >= 1024 && config.max_record_bytes <= 1024 * 1024,
            "Trace record bound must be 1KiB..1MiB"
        );
        ensure!(
            config.max_file_bytes >= config.max_record_bytes as u64,
            "Trace file bound must fit one record"
        );
        ensure!(
            (1..=16).contains(&config.retained_files),
            "Trace retention must be 1..16 files"
        );
        ensure!(
            config.max_payload_bytes <= config.max_record_bytes,
            "Trace payload bound exceeds record bound"
        );
        ensure!(config.label.len() <= 1024, "Trace label exceeds 1024 bytes");
        if let Some(parent) = config.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.path)
            .context("Open trace output")?;
        let bytes = file.metadata()?.len();
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let (done_tx, done) = mpsc::sync_channel(1);
        let counters = Arc::new(Counters {
            diagnostics,
            ..Counters::default()
        });
        let writer_counters = counters.clone();
        let writer_config = config.clone();
        let writer = thread::Builder::new()
            .name("bubblepaw-trace".into())
            .spawn(move || {
                writer_loop(writer_config, file, bytes, receiver, &writer_counters);
                let _ = done_tx.send(());
            })?;
        let trace = Self(Arc::new(Inner {
            config,
            sender: Mutex::new(Some(sender)),
            counters,
            done: Mutex::new(done),
            writer: Mutex::new(Some(writer)),
        }));
        trace.runtime_info(json!({"event":"trace_started","mode":trace.0.config.mode,"max_record_bytes":trace.0.config.max_record_bytes,"max_payload_bytes":trace.0.config.max_payload_bytes,"max_file_bytes":trace.0.config.max_file_bytes,"retained_files":trace.0.config.retained_files,"queue_capacity":trace.0.config.queue_capacity}));
        Ok(trace)
    }
    pub fn observation(&self) -> Observation {
        Observation {
            sink: Arc::new(self.clone()),
            mode: self.0.config.mode,
            max_payload_bytes: self.0.config.max_payload_bytes,
        }
    }
    /// Supply non-secret build/provider/shell metadata. This never captures a content payload.
    pub fn runtime_info(&self, metrics: Value) {
        self.record(ObservationRecord {
            created_at: String::new(),
            kind: "runtime_info".into(),
            scope: ObservationScope::default(),
            metrics,
            payload: None,
            payload_truncated: false,
        });
    }
    pub fn status(&self) -> TraceStatus {
        self.0.counters.snapshot()
    }
    pub fn flush(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let (ack, receiver) = mpsc::sync_channel(1);
        let mut item = QueueItem::Flush(ack);
        loop {
            let result = self
                .0
                .sender
                .lock()
                .unwrap()
                .as_ref()
                .ok_or_else(|| anyhow!("Trace already finished"))?
                .try_send(item);
            match result {
                Ok(()) => break,
                Err(TrySendError::Full(value)) => {
                    item = value;
                    ensure!(Instant::now() < deadline, "Trace flush queue timed out");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(anyhow!("Trace writer disconnected"));
                }
            }
        }
        receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("Trace flush timed out")?;
        ensure!(
            self.status().write_errors == 0,
            "Trace writer reported errors: {:?}",
            self.status().last_error
        );
        Ok(())
    }
    pub fn finish(&self, timeout: Duration) -> Result<()> {
        self.0.sender.lock().unwrap().take();
        let mut writer = self.0.writer.lock().unwrap();
        if writer.is_none() {
            return Ok(());
        }
        self.0
            .done
            .lock()
            .unwrap()
            .recv_timeout(timeout)
            .context("Trace shutdown timed out; writer detached until filesystem responds")?;
        if let Some(handle) = writer.take() {
            handle
                .join()
                .map_err(|_| anyhow!("Trace writer panicked"))?;
        }
        ensure!(
            self.status().write_errors == 0,
            "Trace writer reported errors: {:?}",
            self.status().last_error
        );
        Ok(())
    }
}
impl ObservationSink for JsonlTrace {
    fn record(&self, mut record: ObservationRecord) {
        // Defend metadata mode at the adapter boundary too.
        if self.0.config.mode == ObservationMode::Metadata {
            record.payload = None;
            record.payload_truncated = false;
        }
        // Bound queued records too, including metadata supplied by a host.
        let bound = self.0.config.max_record_bytes;
        if serde_json::to_vec(&record).map_or(true, |bytes| bytes.len() > bound) {
            record.payload = None;
            record.payload_truncated = true;
            if serde_json::to_vec(&record).map_or(true, |bytes| bytes.len() > bound) {
                self.0.counters.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        let sender = self.0.sender.lock().unwrap();
        match sender
            .as_ref()
            .map(|s| s.try_send(QueueItem::Record(Box::new(record))))
        {
            Some(Ok(())) => {
                self.0.counters.accepted.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.0.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overload_is_nonblocking_counted_and_flush_has_a_deadline() -> Result<()> {
        // Hold the receiver without consuming, making overload deterministic.
        let (sender, _receiver) = mpsc::sync_channel(1);
        let (_done_sender, done) = mpsc::sync_channel(1);
        let trace = JsonlTrace(Arc::new(Inner {
            config: TraceConfig::default(),
            sender: Mutex::new(Some(sender)),
            counters: Arc::new(Counters::default()),
            done: Mutex::new(done),
            writer: Mutex::new(None),
        }));
        for _ in 0..5 {
            trace.runtime_info(json!({"test":true}));
        }
        assert_eq!(trace.status().accepted, 1);
        assert_eq!(trace.status().dropped, 4);
        let started = Instant::now();
        assert!(trace.flush(Duration::from_millis(10)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }
    #[test]
    fn oversized_metadata_never_enters_the_bounded_queue() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (_done_sender, done) = mpsc::sync_channel(1);
        let trace = JsonlTrace(Arc::new(Inner {
            config: TraceConfig {
                max_record_bytes: 1024,
                ..Default::default()
            },
            sender: Mutex::new(Some(sender)),
            counters: Arc::new(Counters::default()),
            done: Mutex::new(done),
            writer: Mutex::new(None),
        }));
        trace.runtime_info(json!({"large":"x".repeat(4096)}));
        assert_eq!(trace.status().dropped, 1);
        assert!(receiver.try_recv().is_err());
    }
}
fn rotation(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    name.into()
}
struct Output {
    config: TraceConfig,
    writer: Option<BufWriter<File>>,
    bytes: u64,
    trace_id: String,
    sequence: u64,
}
impl Output {
    fn flush(&mut self) -> Result<()> {
        if let Some(writer) = &mut self.writer {
            writer.flush()?;
        }
        Ok(())
    }
    fn write(&mut self, mut record: ObservationRecord, counters: &Counters) -> Result<()> {
        if self.writer.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.config.path)?;
            self.bytes = file.metadata()?.len();
            self.writer = Some(BufWriter::new(file));
        }
        self.sequence += 1;
        let envelope = |record: &ObservationRecord| json!({"schema_version":1,"trace_id":self.trace_id,"sequence":self.sequence,"label":self.config.label,"record":record});
        let mut bytes = serde_json::to_vec(&envelope(&record))?;
        if bytes.len() + 1 > self.config.max_record_bytes {
            record.payload = None;
            record.payload_truncated = true;
            bytes = serde_json::to_vec(&envelope(&record))?;
        }
        if bytes.len() + 1 > self.config.max_record_bytes {
            counters.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if self.bytes + bytes.len() as u64 + 1 > self.config.max_file_bytes {
            self.flush()?;
            // Close the old handle before renaming it on Windows.
            self.writer.take();
            if self.config.retained_files > 1 {
                for index in (1..self.config.retained_files).rev() {
                    let target = rotation(&self.config.path, index);
                    if target.exists() {
                        std::fs::remove_file(&target)?;
                        if index == self.config.retained_files - 1 {
                            counters.retention_evictions.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let source = if index == 1 {
                        self.config.path.clone()
                    } else {
                        rotation(&self.config.path, index - 1)
                    };
                    if source.exists() {
                        std::fs::rename(source, target)?;
                    }
                }
            } else {
                counters.retention_evictions.fetch_add(1, Ordering::Relaxed);
            }
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.config.path)?;
            self.writer = Some(BufWriter::new(file));
            self.bytes = 0;
            counters.rotations.fetch_add(1, Ordering::Relaxed);
        }
        bytes.push(b'\n');
        self.writer
            .as_mut()
            .ok_or_else(|| anyhow!("Trace file unavailable"))?
            .write_all(&bytes)?;
        self.bytes += bytes.len() as u64;
        counters.written.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn status(&mut self, counters: &Counters) -> Result<()> {
        self.write(
            ObservationRecord {
                created_at: String::new(),
                kind: "trace_status".into(),
                scope: ObservationScope::default(),
                metrics: serde_json::to_value(counters.snapshot())?,
                payload: None,
                payload_truncated: false,
            },
            counters,
        )?;
        self.flush()?;
        Ok(())
    }
}
fn writer_loop(
    config: TraceConfig,
    file: File,
    bytes: u64,
    receiver: Receiver<QueueItem>,
    counters: &Counters,
) {
    let mut output = Output {
        config,
        writer: Some(BufWriter::new(file)),
        bytes,
        trace_id: uuid::Uuid::new_v4().to_string(),
        sequence: 0,
    };
    let mut reported_health = (0, 0, 0, 0);
    let mut stderr_error = None;
    loop {
        let first = match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(value) => value,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(error) = output.flush() {
                    counters.error(&error);
                }
                report_health(
                    &mut output,
                    counters,
                    &mut reported_health,
                    &mut stderr_error,
                );
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut items = vec![first];
        for _ in 1..64 {
            match receiver.try_recv() {
                Ok(value) => items.push(value),
                Err(_) => break,
            }
        }
        for item in items {
            let result = match item {
                QueueItem::Record(record) => output.write(*record, counters),
                QueueItem::Flush(ack) => {
                    let result = output.status(counters);
                    if let Err(error) = &result {
                        counters.error(error);
                    }
                    let _ = ack.send(());
                    continue;
                }
            };
            if let Err(error) = result {
                counters.error(&error);
                counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Err(error) = output.flush() {
            counters.error(&error);
        }
        report_health(
            &mut output,
            counters,
            &mut reported_health,
            &mut stderr_error,
        );
    }
    if let Err(error) = output.status(counters) {
        counters.error(&error);
    }
}
fn report_health(
    output: &mut Output,
    counters: &Counters,
    reported: &mut (u64, u64, u64, u64),
    stderr_error: &mut Option<String>,
) {
    let current = counters.snapshot();
    if (
        current.dropped,
        current.write_errors,
        current.rotations,
        current.retention_evictions,
    ) == *reported
    {
        return;
    }
    if let Err(error) = output.status(counters) {
        counters.error(&error);
        let message = error.to_string();
        if stderr_error.as_ref() != Some(&message) {
            let _ = counters.diagnostics.stderr(format!(
                "Trace writer could not persist loss status: {message}\n"
            ));
            *stderr_error = Some(message);
        }
    } else {
        *stderr_error = None;
    }
    let latest = counters.snapshot();
    *reported = (
        latest.dropped,
        latest.write_errors,
        latest.rotations,
        latest.retention_evictions,
    );
}

/// Prompt-cache totals for one provider/model, from records that carry `cache_group`.
/// Null numbers count as zero.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CacheAggregate {
    /// Model requests with cache accounting (main-loop and episode calls).
    pub requests: u64,
    pub input_tokens: u64,
    pub measured_cache_read_tokens: u64,
    pub expected_cacheable_tokens: u64,
    pub unexplained_miss_tokens: u64,
    /// Main-loop prefix breaks the harness declared, from `cache_bust` records.
    pub declared_busts: u64,
    /// Prefix breaks the harness did not declare, from `cache_bust` records.
    pub undeclared_busts: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TraceReport {
    pub records: u64,
    pub model_calls: u64,
    pub model_errors: u64,
    pub preparation_errors: u64,
    pub run_errors: u64,
    pub tool_errors: u64,
    /// Per result observation, so repeated polls can count the same process exit again.
    pub process_observations: u64,
    pub process_nonzero_exits: u64,
    pub process_statuses: BTreeMap<String, u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model_calls_without_usage: u64,
    pub model_latency_ms: u64,
    pub tool_latency_ms: u64,
    pub first_text_latency_ms: u64,
    pub first_text_observations: u64,
    pub tool_calls: BTreeMap<String, u64>,
    pub calls_per_execution: BTreeMap<String, u64>,
    pub command_kinds: BTreeMap<String, u64>,
    pub command_errors: BTreeMap<String, u64>,
    pub program_attempts: BTreeMap<String, u64>,
    pub completed_program_calls: BTreeMap<String, u64>,
    pub script_prefixes: BTreeMap<String, u64>,
    pub execution_statuses: BTreeMap<String, u64>,
    pub execution_errors: u64,
    pub outputs: u64,
    pub accepted_inputs: BTreeMap<String, u64>,
    pub cancel_intents: u64,
    pub template_parts: u64,
    pub template_values: u64,
    pub repeated_inputs: BTreeMap<String, u64>,
    pub labels: BTreeMap<String, u64>,
    pub dropped_records: u64,
    pub write_errors: u64,
    pub rotations: u64,
    pub retention_evictions: u64,
    pub incomplete_calls: u64,
    pub malformed_records: u64,
    /// Prompt-cache accounting keyed "provider/model" (the model alone when the provider is empty).
    pub cache: BTreeMap<String, CacheAggregate>,
}
/// Provider/model key; the model name alone when the provider is unknown.
fn cache_key(provider: &str, model: &str) -> String {
    if provider.is_empty() {
        model.to_owned()
    } else {
        format!("{provider}/{model}")
    }
}
/// Add one request's cache numbers from its `cache_group` and metrics.
fn add_cache_request(report: &mut TraceReport, group: &Value, metrics: &Value) {
    let key = cache_key(
        group["provider"].as_str().unwrap_or(""),
        group["model"].as_str().unwrap_or(""),
    );
    let entry = report.cache.entry(key).or_default();
    entry.requests += 1;
    entry.input_tokens += metrics["usage"]["input_tokens"].as_u64().unwrap_or(0);
    entry.measured_cache_read_tokens += metrics["measured_cache_read_tokens"].as_u64().unwrap_or(0);
    entry.expected_cacheable_tokens += metrics["expected_cacheable_tokens"].as_u64().unwrap_or(0);
    entry.unexplained_miss_tokens += metrics["unexplained_miss_tokens"].as_u64().unwrap_or(0);
}
/// Aggregate a bounded rotation set. Repeats mean exact serialized inputs, not semantic equivalence.
pub fn read_report(path: impl AsRef<Path>) -> Result<TraceReport> {
    let path = path.as_ref();
    let mut files: Vec<_> = (1..16)
        .rev()
        .map(|i| rotation(path, i))
        .filter(|p| p.is_file())
        .collect();
    if path.exists() {
        files.push(path.to_path_buf());
    }
    ensure!(!files.is_empty(), "No trace files found");
    let mut report = TraceReport::default();
    let mut fingerprints = BTreeMap::<String, u64>::new();
    let mut pending = BTreeMap::<String, (String, Option<String>)>::new();
    let mut gaps = BTreeMap::<String, (u64, u64, u64, u64)>::new();
    for file in files {
        for line in BufReader::new(File::open(file)?).lines() {
            let line = line?;
            let envelope: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => {
                    report.malformed_records += 1;
                    continue;
                }
            };
            let trace_id = envelope["trace_id"].as_str().unwrap_or("");
            let record: ObservationRecord = match serde_json::from_value(envelope["record"].clone())
            {
                Ok(v) => v,
                Err(_) => {
                    report.malformed_records += 1;
                    continue;
                }
            };
            report.records += 1;
            *report
                .labels
                .entry(envelope["label"].as_str().unwrap_or("").into())
                .or_default() += 1;
            let metrics = &record.metrics;
            let call = format!(
                "{}:{}",
                trace_id,
                record.scope.call_id.as_deref().unwrap_or("")
            );
            match record.kind.as_str() {
                "model_started" => {
                    report.model_calls += 1;
                }
                "model_failed" => {
                    report.model_errors += 1;
                    report.model_calls_without_usage += 1;
                }
                "model_prepare_failed" => {
                    report.preparation_errors += 1;
                }
                "run_failed" => {
                    report.run_errors += 1;
                }
                "model_finished" => {
                    report.model_latency_ms += metrics["elapsed_ms"].as_u64().unwrap_or(0);
                    if let Some(first) = metrics["first_text_ms"].as_u64() {
                        report.first_text_latency_ms += first;
                        report.first_text_observations += 1;
                    }
                    if metrics["usage"].is_object() {
                        report.input_tokens +=
                            metrics["usage"]["input_tokens"].as_u64().unwrap_or(0);
                        report.output_tokens +=
                            metrics["usage"]["output_tokens"].as_u64().unwrap_or(0);
                    } else {
                        report.model_calls_without_usage += 1;
                    }
                    if metrics["cache_group"].is_object() {
                        add_cache_request(&mut report, &metrics["cache_group"], metrics);
                    }
                }
                "runtime_info"
                    if metrics["event"] == "episode_model_call"
                        && metrics["cache_group"].is_object() =>
                {
                    add_cache_request(&mut report, &metrics["cache_group"], metrics);
                }
                "cache_bust" => {
                    let key = cache_key(
                        metrics["provider"].as_str().unwrap_or(""),
                        metrics["model"].as_str().unwrap_or(""),
                    );
                    let entry = report.cache.entry(key).or_default();
                    match metrics["declared"].as_bool() {
                        Some(true) => entry.declared_busts += 1,
                        Some(false) => entry.undeclared_busts += 1,
                        None => {}
                    }
                }
                "tool_started" => {
                    let tool = metrics["tool"].as_str().unwrap_or("unknown");
                    *report.tool_calls.entry(tool.into()).or_default() += 1;
                    if let Some(execution) = &record.scope.execution_id {
                        *report
                            .calls_per_execution
                            .entry(execution.clone())
                            .or_default() += 1;
                    }
                    let fingerprint = metrics["input_fingerprint"].as_str().unwrap_or("");
                    *fingerprints
                        .entry(format!("{tool}:{fingerprint}"))
                        .or_default() += 1;
                    let kind = metrics["command_kind"].as_str().unwrap_or("none");
                    if kind != "none" {
                        *report.command_kinds.entry(kind.into()).or_default() += 1;
                    }
                    let program = metrics["program_name"].as_str().map(str::to_string);
                    if let Some(program) = &program {
                        *report.program_attempts.entry(program.clone()).or_default() += 1;
                    }
                    if let Some(prefix) = metrics["script_prefix"].as_str() {
                        *report.script_prefixes.entry(prefix.into()).or_default() += 1;
                    }
                    report.template_parts += metrics["template_parts"].as_u64().unwrap_or(0);
                    report.template_values += metrics["template_values"].as_u64().unwrap_or(0);
                    pending.insert(call, (kind.into(), program));
                }
                "tool_finished" => {
                    let started = pending.remove(&call);
                    report.tool_latency_ms += metrics["elapsed_ms"].as_u64().unwrap_or(0);
                    if let Some(process) = metrics["process"].as_object() {
                        report.process_observations += 1;
                        if process
                            .get("exit_code")
                            .and_then(Value::as_i64)
                            .is_some_and(|code| code != 0)
                        {
                            report.process_nonzero_exits += 1;
                        }
                        if let Some(status) = process.get("status").and_then(Value::as_str) {
                            *report.process_statuses.entry(status.into()).or_default() += 1;
                        }
                    }
                    if metrics["is_error"].as_bool() == Some(true) {
                        report.tool_errors += 1;
                        if let Some((kind, _)) = started
                            && kind != "none"
                        {
                            *report.command_errors.entry(kind).or_default() += 1;
                        }
                    } else if let Some((_, Some(program))) = started {
                        *report.completed_program_calls.entry(program).or_default() += 1;
                    }
                }
                "tool_failed" => {
                    if let Some((kind, _)) = pending.remove(&call)
                        && kind != "none"
                    {
                        *report.command_errors.entry(kind).or_default() += 1;
                    }
                    report.tool_latency_ms += metrics["elapsed_ms"].as_u64().unwrap_or(0);
                    report.tool_errors += 1;
                }
                "output_emitted" => {
                    report.outputs += 1;
                    if metrics["output_type"] == "execution"
                        && let Some(status) = metrics["status"].as_str()
                    {
                        *report.execution_statuses.entry(status.into()).or_default() += 1;
                        if status == "failed" {
                            report.execution_errors += 1;
                        }
                    }
                }
                "input_accepted" => {
                    *report
                        .accepted_inputs
                        .entry(metrics["input_kind"].as_str().unwrap_or("unknown").into())
                        .or_default() += 1;
                }
                "cancel_intent" => {
                    report.cancel_intents += 1;
                }
                "trace_status" => {
                    gaps.insert(
                        trace_id.into(),
                        (
                            metrics["dropped"].as_u64().unwrap_or(0),
                            metrics["write_errors"].as_u64().unwrap_or(0),
                            metrics["rotations"].as_u64().unwrap_or(0),
                            metrics["retention_evictions"].as_u64().unwrap_or(0),
                        ),
                    );
                }
                _ => {}
            }
        }
    }
    report.incomplete_calls = pending.len() as u64;
    report.repeated_inputs = fingerprints
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .collect();
    report.dropped_records = gaps.values().map(|(d, _, _, _)| d).sum();
    report.write_errors = gaps.values().map(|(_, e, _, _)| e).sum();
    report.rotations = gaps.values().map(|(_, _, r, _)| r).sum();
    report.retention_evictions = gaps.values().map(|(_, _, _, e)| e).sum();
    Ok(report)
}
