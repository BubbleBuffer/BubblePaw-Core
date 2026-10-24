use super::workspace::{WorkspaceTools, optional_string, string, unsigned, validate_fields};
use crate::command_template::{CommandExecution, prepare};
use crate::config::ExecConfig;
use crate::wake::WakeCoordinator;
use anyhow::{Result, anyhow, bail, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Store, Tool, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry,
    ToolResult,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Notify,
};

const RETAIN_BYTES: usize = 32 * 1024;
const MAX_YIELD_MS: u64 = 30_000;
const REPLY_BYTES: usize = 16 * 1024;
const DEFAULT_WAIT_MS: u64 = 600_000;

fn exec_yield_ms(input: &Value, sync_ms: u64) -> Result<u64> {
    Ok(unsigned(input, "yieldMs", None)?
        .unwrap_or(sync_ms.max(unsigned(input, "timeoutMs", Some(0))?.unwrap())))
}

fn process_wait_ms(input: &Value) -> Result<u64> {
    Ok(unsigned(input, "timeoutMs", Some(DEFAULT_WAIT_MS))?.unwrap())
}

/// A bounded explanation shared by exec/poll/wait. Raw stdout stays in its
/// existing capture fields; only exceptional outcomes need this extra context.
fn process_diagnostic(value: &Value) -> Option<String> {
    let status = value["status"].as_str().unwrap_or("exited");
    let exit = value["exitCode"].as_i64();
    let failed = matches!(status, "timed_out" | "cancelled" | "timeout")
        || (status == "exited" && exit != Some(0));
    let mut capture_notes = Vec::new();
    let mut decoding_notes = Vec::new();
    let complete_spill = value.get("fullOutput").is_some();
    for stream in ["stdout", "stderr"] {
        let capture = &value["capture"][stream];
        let omitted: u64 = capture["gaps"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|gap| {
                gap["to"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_sub(gap["from"].as_u64().unwrap_or(0))
            })
            .sum();
        if capture["lossyUtf8"] == true {
            decoding_notes.push(format!("{stream}: decoding is lossy (invalid UTF-8)"));
        }
        if (omitted > 0 && !complete_spill) || capture["captureError"] == true {
            capture_notes.push(format!(
                "{stream}: {} total bytes, {omitted} omitted bytes{}",
                capture["totalBytes"].as_u64().unwrap_or(0),
                if capture["captureError"] == true {
                    "; pipe capture error"
                } else {
                    ""
                }
            ));
        }
    }
    if !failed && capture_notes.is_empty() && decoding_notes.is_empty() && !complete_spill {
        return None;
    }
    let mut explanation = match status {
        "exited" if exit == Some(0) => "Command succeeded (exit code 0).".to_owned(),
        "exited" => match exit {
            Some(code) => format!("Command exited with code {code}. Check the command and its arguments; nonzero exits can also mean no matches."),
            None => "Command terminated without an exit code.".to_owned(),
        },
        "timed_out" => "Command timed out and was terminated; increase timeoutMs only if appropriate.".to_owned(),
        "cancelled" => "Command was cancelled and terminated.".to_owned(),
        "timeout" => "Wait timed out; the command is still running. Poll or wait again using the same handle.".to_owned(),
        _ => "Command is still running.".to_owned(),
    };
    if complete_spill {
        explanation.push_str(" Displayed output is a head/tail preview; complete decoded output is saved at fullOutput.path. Use read with line ranges.");
    }
    if !capture_notes.is_empty() {
        explanation.push_str(&format!(" Capture is incomplete ({}). Omitted output does not indicate command failure. Request smaller output or batches, or redirect full output to a file and use file({{path}}).", capture_notes.join("; ")));
    }
    if !decoding_notes.is_empty() {
        explanation.push_str(&format!(" {}. The decoded text is altered; do not treat it as exact output or complete JSON. Redirect raw output to a file to preserve the bytes.", decoding_notes.join("; ")));
    }
    if failed {
        let stderr = value["stderr"].as_str().unwrap_or("");
        if !stderr.trim().is_empty() {
            let preview: String = stderr.chars().take(768).collect();
            explanation.push_str(&format!(" stderr: {preview}"));
            if stderr.chars().count() > 768 {
                explanation.push_str(" [stderr preview shortened]");
            }
        }
    }
    Some(explanation)
}

fn with_process_diagnostic(mut value: Value) -> Value {
    if let Some(diagnostic) = process_diagnostic(&value) {
        value["diagnostic"] = json!(diagnostic);
    }
    value
}

#[path = "process_reminders.rs"]
mod reminder_notices;

type SharedEnvironment = Arc<RwLock<Option<Vec<(String, String)>>>>;

/// Host-owned pipe processes. Pipe output is transient, with bounded previews
/// and an optional bounded disk spool; no output chunks enter the chat store.
pub struct ProcessTools {
    workspace: WorkspaceTools,
    config: Arc<RwLock<ExecConfig>>,
    /// Client environment for native commands; `None` inherits the host's.
    inherited_env: SharedEnvironment,
    diagnostics: Arc<OnceLock<crate::cli_output::Output>>,
    spills: Option<crate::spill::SpillStore>,
    spool_root: Option<PathBuf>,
    /// Held for the host's lifetime; other hosts sweep only unlocked spools.
    _spool_lock: Option<File>,
    sessions: Arc<Mutex<HashMap<String, Arc<ProcessSession>>>>,
    notices: Arc<WakeCoordinator>,
    wake: Arc<OnceLock<Arc<WakeCoordinator>>>,
    store: Arc<OnceLock<Store>>,
    recovered: Arc<Mutex<std::collections::HashSet<String>>>,
    /// A workspace view keeps the host spool lock and job lifetime alive.
    _owner: Option<Arc<ProcessTools>>,
}
struct ProcessSession {
    store: Option<Store>,
    diagnostics: crate::cli_output::Output,
    retired: AtomicBool,
    finalized: AtomicBool,
    owner: String,
    handle: String,
    invocation_id: String,
    command: String,
    created: Instant,
    stop: CancellationToken,
    stdout: Arc<Mutex<Output>>,
    stderr: Arc<Mutex<Output>>,
    read_cursor: Mutex<(u64, u64)>,
    state: Mutex<ProcessState>,
    changed: Notify,
    foreground_changed: Notify,
    spool_paths: Vec<PathBuf>,
    full_output: Mutex<Option<Value>>,
    tree: bubblepaw_process::ProcessTree,
}
#[derive(Default)]
struct ProcessState {
    status: Option<ProcessStatus>,
    /// Set once a tool result returned the terminal status to the model. Runtime
    /// notes list a finished command only while this is false.
    delivered: bool,
    backgrounded: bool,
    sync_waiting: bool,
    background_reason: Option<&'static str>,
    notice_queued: bool,
}
#[derive(Clone)]
struct ProcessStatus {
    exit_code: Option<i32>,
    cancelled: bool,
    timed_out: bool,
    /// Set only when the shell's exit handler reported a directory change.
    note: Option<String>,
}
impl ProcessStatus {
    fn name(&self) -> &'static str {
        if self.cancelled {
            "cancelled"
        } else if self.timed_out {
            "timed_out"
        } else {
            "exited"
        }
    }
}
#[derive(Default)]
struct Output {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
    error: bool,
    spool: Option<File>,
    spool_bytes: u64,
    spool_budget: Option<Arc<Mutex<usize>>>,
}
impl Output {
    fn full_text(&mut self) -> Result<Option<String>> {
        if self.error {
            return Ok(None);
        }
        if self.total == self.head.len() as u64 {
            return Ok(Some(String::from_utf8_lossy(&self.head).into_owned()));
        }
        if self.spool_bytes != self.total {
            return Ok(None);
        }
        let Some(spool) = self.spool.as_mut() else {
            return Ok(None);
        };
        spool.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        spool.take(self.total).read_to_end(&mut bytes)?;
        // Reading a spool must not change where a later append would write.
        self.spool.as_mut().unwrap().seek(SeekFrom::End(0))?;
        ensure!(
            bytes.len() as u64 == self.total,
            "process spool is incomplete"
        );
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
    fn append(&mut self, bytes: &[u8]) {
        if let Some(spool) = self.spool.as_mut()
            && let Some(budget) = &self.spool_budget
        {
            match budget.lock() {
                Ok(mut budget) => {
                    let count = bytes.len().min(*budget);
                    if count > 0 {
                        match spool.write_all(&bytes[..count]) {
                            Ok(()) => {
                                self.spool_bytes += count as u64;
                                *budget -= count;
                            }
                            Err(_) => {
                                self.error = true;
                                *budget = 0;
                            }
                        }
                    }
                }
                Err(_) => self.error = true,
            }
        }
        self.total = self.total.saturating_add(bytes.len() as u64);
        let keep = bytes
            .len()
            .min(RETAIN_BYTES.saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..keep]);
        if bytes.len() >= RETAIN_BYTES {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - RETAIN_BYTES..]);
        } else {
            while self.tail.len() + bytes.len() > RETAIN_BYTES {
                self.tail.pop_front();
            }
            self.tail.extend(bytes);
        }
    }
    fn since(&mut self, cursor: u64) -> Result<(String, Value)> {
        ensure!(
            cursor <= self.total,
            "output cursor exceeds captured stream length"
        );
        let head_end = self.head.len() as u64;
        let tail_start = self
            .total
            .saturating_sub(self.tail.len() as u64)
            .max(head_end);
        let retained_start = self.total.saturating_sub(self.tail.len() as u64);
        let mut retained = Vec::new();
        if cursor < head_end {
            retained.push((cursor, head_end));
        }
        if cursor.max(tail_start) < self.total {
            retained.push((cursor.max(tail_start), self.total));
        }
        let available = retained.iter().map(|(from, to)| to - from).sum::<u64>();
        // Reserve space for visible omission markers without expanding the reply.
        let payload_budget = if available > REPLY_BYTES as u64 || available < self.total - cursor {
            REPLY_BYTES - 256
        } else {
            REPLY_BYTES
        } as u64;
        let mut selected = Vec::new();
        if available <= payload_budget {
            selected = retained;
        } else {
            let mut offset = 0;
            for (from, to) in retained {
                let len = to - from;
                let prefix = (payload_budget / 2).saturating_sub(offset).min(len);
                if prefix > 0 {
                    selected.push((from, from + prefix));
                }
                let suffix = (available - payload_budget / 2)
                    .saturating_sub(offset)
                    .min(len);
                if suffix < len {
                    selected.push((from + suffix, to));
                }
                offset += len;
            }
        }
        let mut bytes = Vec::new();
        let mut gaps = Vec::new();
        let mut position = cursor;
        for (from, to) in selected {
            if position < from {
                gaps.push(json!({"from":position,"to":from}));
                bytes.extend_from_slice(
                    format!("\n[... omitted bytes {position}..{from} ...]\n").as_bytes(),
                );
            }
            if to <= head_end {
                bytes.extend_from_slice(&self.head[from as usize..to as usize]);
            } else {
                bytes.extend(
                    self.tail
                        .iter()
                        .skip((from - retained_start) as usize)
                        .take((to - from) as usize),
                );
            }
            position = to;
        }
        if position < self.total {
            gaps.push(json!({"from":position,"to":self.total}));
            bytes.extend_from_slice(
                format!("\n[... omitted bytes {position}..{} ...]\n", self.total).as_bytes(),
            );
        }
        Ok((
            String::from_utf8_lossy(&bytes).into_owned(),
            json!({"gaps":gaps,"captureError":self.error,"totalBytes":self.total,"lossyUtf8":std::str::from_utf8(&bytes).is_err(),"spoolRetainedBytes":self.spool_bytes,"spoolGaps":if self.spool.is_some() && self.spool_bytes < self.total {vec![json!({"from":self.spool_bytes,"to":self.total})]}else{Vec::new()}}),
        ))
    }
    fn remove_spool(&mut self) {
        self.spool = None;
        self.spool_bytes = 0;
        self.spool_budget = None;
    }
}
impl ProcessTools {
    pub fn new(workspace: WorkspaceTools) -> Self {
        Self {
            workspace,
            config: Arc::new(RwLock::new(ExecConfig::default())),
            inherited_env: Arc::default(),
            diagnostics: Arc::default(),
            spills: None,
            spool_root: None,
            _spool_lock: None,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            notices: Arc::default(),
            wake: Arc::default(),
            store: Arc::default(),
            recovered: Arc::default(),
            _owner: None,
        }
    }
    /// Rebind file tools while retaining session read grants and spill access.
    pub(crate) fn workspace_for(&self, root: &Path) -> Result<WorkspaceTools> {
        self.workspace.for_workspace(root)
    }
    /// Bind command paths to another workspace while retaining host-wide job
    /// admission, configuration, output storage and completion ownership.
    pub fn for_workspace(self: &Arc<Self>, workspace: WorkspaceTools) -> Self {
        Self {
            workspace,
            config: self.config.clone(),
            inherited_env: self.inherited_env.clone(),
            diagnostics: self.diagnostics.clone(),
            spills: self.spills.clone(),
            spool_root: self.spool_root.clone(),
            _spool_lock: None,
            sessions: self.sessions.clone(),
            notices: self.notices.clone(),
            wake: self.wake.clone(),
            store: self.store.clone(),
            recovered: self.recovered.clone(),
            _owner: Some(self.clone()),
        }
    }
    /// This constructor is a host startup boundary: old process handles cannot
    /// survive restart, so their disposable spool is swept before registration.
    pub fn with_config(
        workspace: WorkspaceTools,
        config: ExecConfig,
        data_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        fs::create_dir_all(data_dir.as_ref())?;
        let data_dir = fs::canonicalize(data_dir.as_ref())?;
        let jobs = data_dir.join("jobs");
        if let Ok(metadata) = fs::symlink_metadata(&jobs) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "jobs spool must not be a symlink"
            );
            ensure!(metadata.is_dir(), "jobs spool must be a directory");
        }
        fs::create_dir_all(&jobs)?;
        sweep_spools(&jobs);
        // Each host owns one locked spool directory, so several hosts can share
        // a data directory without deleting each other's live output.
        let spool_root = jobs.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&spool_root)?;
        let lock = File::create(spool_root.join(".lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|error| anyhow!("Locking jobs spool: {error}"))?;
        let mut processes = Self::new(workspace);
        processes.apply_config(config);
        processes.spills = Some(crate::spill::SpillStore::new(&data_dir)?);
        processes.spool_root = Some(spool_root);
        processes._spool_lock = Some(lock);
        Ok(processes)
    }
    /// Apply settings to future exec calls. Admitted jobs retain their own
    /// foreground deadline, hard timeout and output spool budget.
    pub fn apply_config(&self, config: ExecConfig) {
        *self.config.write().expect("exec config lock") = config;
    }
    /// Start native commands launched from now on with exactly this environment,
    /// as a standalone run from the invoking client would see it. `None` restores
    /// inheritance from the host process. Workspace views share the setting.
    pub fn set_inherited_env(&self, env: Option<Vec<(String, String)>>) {
        *self.inherited_env.write().expect("inherited env lock") = env;
    }

    pub fn set_diagnostics(&self, output: crate::cli_output::Output) {
        let _ = self.diagnostics.set(output);
    }
    pub fn set_skill_read_roots(&self, session: &str, roots: Vec<PathBuf>) -> Result<()> {
        self.workspace.set_skill_read_roots(session, roots)
    }
    fn config(&self) -> ExecConfig {
        self.config.read().expect("exec config lock").clone()
    }
    /// Attach the host's shared completion inbox before admitting jobs.
    pub fn set_wake(&self, wake: Arc<WakeCoordinator>) {
        self.try_set_wake(wake)
            .expect("process host uses another wake coordinator");
    }
    pub fn try_set_wake(&self, wake: Arc<WakeCoordinator>) -> Result<()> {
        let existing = self.wake.get_or_init(|| wake.clone());
        ensure!(
            Arc::ptr_eq(existing, &wake),
            "process host uses another wake coordinator"
        );
        Ok(())
    }
    /// Preserve known terminal completion metadata for next-turn recovery.
    /// Restoration never executes a command or proactively wakes a session.
    pub fn set_store(&self, store: Store) {
        let _ = self.store.set(store);
    }
    pub fn wake_coordinator(&self) -> Arc<WakeCoordinator> {
        self.wake.get().unwrap_or(&self.notices).clone()
    }
    pub fn jobs(&self, owner: &str) -> Result<Value> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow!("process registry poisoned"))?;
        let mut jobs = Vec::new();
        for (id, session) in sessions
            .iter()
            .filter(|(_, session)| session.owner == owner)
        {
            let state = session
                .state
                .lock()
                .map_err(|_| anyhow!("process status poisoned"))?;
            let stdout = session
                .stdout
                .lock()
                .map_err(|_| anyhow!("stdout poisoned"))?;
            let stderr = session
                .stderr
                .lock()
                .map_err(|_| anyhow!("stderr poisoned"))?;
            jobs.push(json!({"handle":format!("process:{id}"),"invocationId":session.invocation_id,"command":session.command,"status":state.status.as_ref().map_or("running",ProcessStatus::name),"exitCode":state.status.as_ref().and_then(|status|status.exit_code),"stdoutBytes":stdout.total,"stderrBytes":stderr.total,"outputBytes":stdout.total.saturating_add(stderr.total),"spooledBytes":stdout.spool_bytes.saturating_add(stderr.spool_bytes),"ageMs":session.created.elapsed().as_millis().min(u64::MAX as u128) as u64,"backgrounded":state.backgrounded,"foreground":state.sync_waiting}));
        }
        jobs.sort_by(|left, right| left["handle"].as_str().cmp(&right["handle"].as_str()));
        Ok(json!({"jobs":jobs}))
    }
    pub fn background(&self, owner: &str, handle: &str) -> Result<Value> {
        let session = self.session(handle, owner)?;
        set_background(&session, Some("user"), &self.wake_coordinator())?;
        self.snapshot(handle, &session, 0, 0, Some("user"))
    }
    pub fn interrupt_waits(&self, owner: &str) -> usize {
        let Ok(sessions) = self.sessions.lock() else {
            return 0;
        };
        let mut count = 0;
        for session in sessions.values().filter(|session| session.owner == owner) {
            let Ok(mut state) = session.state.lock() else {
                continue;
            };
            if state.sync_waiting && state.status.is_none() && state.background_reason.is_none() {
                state.backgrounded = true;
                state.background_reason = Some("steer");
                count += 1;
                session.foreground_changed.notify_waiters();
            }
        }
        count
    }
    /// Bounded, owner-only facts for transient continuation context. Never reads output.
    pub fn continuation_status(&self, owner: &str) -> Value {
        let Ok(sessions) = self.sessions.lock() else {
            return json!({"available":false});
        };
        // Listed while running or backgrounded, or after an exit whose terminal
        // status no tool result has returned yet. Delivered exits are omitted.
        let mut entries = sessions
            .iter()
            .filter(|(_, session)| session.owner == owner)
            .filter_map(|(id, session)| {
                let (active, status, exit_code, unread) = match session.state.lock() {
                    Ok(state) => (
                        state.status.is_none(),
                        state.status.as_ref().map_or("running", ProcessStatus::name),
                        state.status.as_ref().and_then(|status| status.exit_code),
                        state.status.is_some() && !state.delivered,
                    ),
                    Err(_) => (true, "unavailable", None, false),
                };
                (active || unread).then(|| {
                    (
                        active,
                        session.created,
                        json!({"handle":format!("process:{id}"),"status":status,"exitCode":exit_code,"unread":unread}),
                    )
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));
        let omitted = entries.len().saturating_sub(8);
        let entries = entries
            .into_iter()
            .take(8)
            .map(|(_, _, value)| value)
            .collect::<Vec<_>>();
        json!({"available":true,"entries":entries,"omitted":omitted})
    }
    pub fn register(self: &Arc<Self>, registry: &mut ToolRegistry) -> Result<()> {
        for name in ["exec", "poll", "cancel", "jobs"] {
            registry.register_host(Arc::new(ProcessTool {
                processes: self.clone(),
                name,
            }))?;
        }
        Ok(())
    }
    pub fn register_wait(self: &Arc<Self>, registry: &mut ToolRegistry) -> Result<()> {
        registry.register_host(Arc::new(ProcessTool {
            processes: self.clone(),
            name: "wait",
        }))?;
        Ok(())
    }
    fn session(&self, handle: &str, owner: &str) -> Result<Arc<ProcessSession>> {
        let id = uuid::Uuid::parse_str(handle.strip_prefix("process:").unwrap_or(handle))
            .map_err(|_| anyhow!("unsupported handle; expected process:<uuid> or a bare uuid"))?
            .to_string();
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow!("process registry poisoned"))?;
        let session = sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown or expired process"))?;
        ensure!(
            session.owner == owner,
            "process belongs to another conversation"
        );
        Ok(session.clone())
    }
    fn start(
        &self,
        context: &ToolContext,
        input: &Value,
        config: &ExecConfig,
    ) -> Result<(String, Arc<ProcessSession>)> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        let cwd_arg = optional_string(input, "cwd")?;
        let workdir_arg = optional_string(input, "workdir")?;
        let cwd = self
            .workspace
            .existing(cwd_arg.or(workdir_arg).unwrap_or("."))?;
        if let (Some(_), Some(workdir)) = (cwd_arg, workdir_arg) {
            ensure!(
                cwd == self.workspace.existing(workdir)?,
                "cwd and workdir must refer to the same directory"
            );
        }
        ensure!(cwd.is_dir(), "cwd is not a directory");
        let prepared = prepare(&input["command"])?;
        let display_command = prepared.rendered.chars().take(512).collect::<String>();
        let id = uuid::Uuid::new_v4().to_string();
        // The shell exit handler writes a marker only on a directory change;
        // the marker shares the job's existing spool lifetime.
        let marker = match (&self.spool_root, cfg!(any(unix, windows))) {
            (Some(root), true) => {
                Some(spool_directory(root, &context.session_id).join(format!("{id}.cwd")))
            }
            _ => None,
        };
        let mut command = {
            let mut command = match prepared.execution {
                CommandExecution::NativeArgv { executable, args } => {
                    let path = std::path::Path::new(&executable);
                    // Windows resolves a relative executable before applying
                    // the child's current_dir. Resolve explicit paths here;
                    // bare program names still use the native PATH lookup.
                    let executable = if path.is_relative() && path.components().count() > 1 {
                        cwd.join(path)
                    } else {
                        path.to_path_buf()
                    };
                    let mut c = Command::new(executable);
                    c.args(args);
                    c
                }
                CommandExecution::Shell { script } => {
                    let script = match &marker {
                        Some(marker) => with_cwd_marker(&script, &cwd, marker),
                        None => script,
                    };
                    #[cfg(windows)]
                    let mut c = Command::new("powershell.exe");
                    #[cfg(windows)]
                    c.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
                    #[cfg(not(windows))]
                    let mut c = Command::new("sh");
                    #[cfg(not(windows))]
                    c.args(["-c", &script]);
                    c
                }
            };
            command.stdin(Stdio::null());
            if let Some(env) = self
                .inherited_env
                .read()
                .expect("inherited env lock")
                .as_ref()
            {
                // The snapshot replaces the host environment: host-only values stay out.
                command.env_clear().envs(env.iter().cloned());
            }
            command
        };
        let timeout = unsigned(input, "timeoutMs", None)?;
        ensure!(timeout != Some(0), "timeoutMs must be positive");
        let sync_ms = config.sync_timeout_secs.saturating_mul(1000);
        let sync_waiting = input.get("background").and_then(Value::as_bool) != Some(true)
            && exec_yield_ms(input, sync_ms)? > 0;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow!("process registry poisoned"))?;
        // The registry lock covers both capacity checking and insertion. The
        // dispatcher still needs a reservation boundary before its effect row.
        let active = sessions
            .values()
            .filter(|session| {
                session
                    .state
                    .lock()
                    .is_ok_and(|state| state.status.is_none())
            })
            .collect::<Vec<_>>();
        if let Some(max) = config.max_jobs {
            ensure!(
                active.len() < max,
                "exec.max_jobs limit ({max}) reached; cancel a running job and retry"
            );
        }
        if let Some(max) = config.max_jobs_per_session {
            ensure!(
                active
                    .iter()
                    .filter(|session| session.owner == context.session_id)
                    .count()
                    < max,
                "exec.max_jobs_per_session limit ({max}) reached; cancel a session job and retry"
            );
        }
        let (stdout_output, stderr_output, mut spool_paths) =
            self.outputs(&context.session_id, &id, config.max_output_bytes)?;
        if let Some(marker) = &marker {
            spool_paths.push(marker.clone());
        }
        command
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let spawned = bubblepaw_process::spawn(&mut command);
        let (mut child, tree) = match spawned {
            Ok(child) => child,
            Err(error) => {
                drop(stdout_output);
                drop(stderr_output);
                for path in &spool_paths {
                    let _ = fs::remove_file(path);
                }
                if let Some(directory) = spool_paths.first().and_then(|path| path.parent()) {
                    let _ = fs::remove_dir(directory);
                }
                return Err(if input["command"].is_object() {
                    anyhow!(
                        "could not launch experimental cmd native executable; shell builtins and aliases require a raw string: {error}"
                    )
                } else {
                    anyhow!("could not launch command: {error}")
                });
            }
        };
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("stdout pipe unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("stderr pipe unavailable"))?;
        let session = Arc::new(ProcessSession {
            store: self.store.get().cloned(),
            diagnostics: self.diagnostics.get().cloned().unwrap_or_default(),
            retired: AtomicBool::new(false),
            finalized: AtomicBool::new(false),
            owner: context.session_id.clone(),
            handle: format!("process:{id}"),
            invocation_id: context.invocation_id.clone(),
            command: display_command,
            created: Instant::now(),
            stop: CancellationToken::new(),
            stdout: Arc::new(Mutex::new(stdout_output)),
            stderr: Arc::new(Mutex::new(stderr_output)),
            read_cursor: Mutex::new((0, 0)),
            state: Mutex::new(ProcessState {
                sync_waiting,
                ..ProcessState::default()
            }),
            changed: Notify::new(),
            foreground_changed: Notify::new(),
            spool_paths,
            full_output: Mutex::new(None),
            tree,
        });
        sessions.insert(id.clone(), session.clone());
        drop(sessions);
        let owned = session.clone();
        let cancellation = context.cancellation.clone();
        let notices = self.wake_coordinator();
        let handle = format!("process:{id}");
        let completed_handle = handle.clone();
        let start = cwd.clone();
        tokio::spawn(async move {
            let stdout_task = tokio::spawn(drain(stdout, owned.stdout.clone()));
            let stderr_task = tokio::spawn(drain(stderr, owned.stderr.clone()));
            let deadline = async {
                match timeout {
                    Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                    None => std::future::pending().await,
                }
            };
            let (exit, cancelled, timed_out) = tokio::select! {
                exit = child.wait() => (exit.ok().and_then(|s| s.code()), false, false),
                _ = owned.stop.cancelled() => (None, true, false),
                _ = cancellation.cancelled() => (None, true, false),
                _ = deadline => (None, false, true),
            };
            // Kill the whole tree once the command ends, so a backgrounded
            // descendant of a normally exiting shell does not outlive its owner.
            // Doing this before draining also releases inherited output pipes.
            owned.tree.kill();
            if cancelled || timed_out {
                let _ = child.kill().await;
            }
            for (mut task, output) in [(stdout_task, &owned.stdout), (stderr_task, &owned.stderr)] {
                if tokio::time::timeout(Duration::from_secs(2), &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                    if let Ok(mut output) = output.lock() {
                        output.error = true;
                    }
                }
            }
            // A killed shell never ran its exit handler, so only a clean exit can
            // report a directory change.
            let note = match &marker {
                Some(marker) if !cancelled && !timed_out => cwd_note(marker, &start),
                _ => None,
            };
            let notice = if let Ok(mut state) = owned.state.lock() {
                state.status = Some(ProcessStatus {
                    exit_code: exit,
                    cancelled,
                    timed_out,
                    note,
                });
                prepare_notice(&mut state)
            } else {
                None
            };
            if let Some(status) = notice {
                publish_notice(&owned, &completed_handle, &status, &notices);
            }
            if cancelled {
                remove_spool(&owned);
            }
            owned.finalized.store(true, Ordering::Release);
            owned.changed.notify_waiters();
            owned.foreground_changed.notify_waiters();
        });
        Ok((handle, session))
    }
    fn outputs(
        &self,
        owner: &str,
        id: &str,
        max_output_bytes: usize,
    ) -> Result<(Output, Output, Vec<PathBuf>)> {
        let Some(root) = &self.spool_root else {
            return Ok((Output::default(), Output::default(), Vec::new()));
        };
        let directory = spool_directory(root, owner);
        fs::create_dir_all(&directory)?;
        let budget = Arc::new(Mutex::new(max_output_bytes));
        let paths = vec![
            directory.join(format!("{id}.out")),
            directory.join(format!("{id}.err")),
        ];
        let open = |path: &Path| {
            fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(path)
        };
        let stdout = open(&paths[0])?;
        let stderr = match open(&paths[1]) {
            Ok(file) => file,
            Err(error) => {
                drop(stdout);
                let _ = fs::remove_file(&paths[0]);
                return Err(error.into());
            }
        };
        Ok((
            Output {
                spool: Some(stdout),
                spool_budget: Some(budget.clone()),
                ..Output::default()
            },
            Output {
                spool: Some(stderr),
                spool_budget: Some(budget),
                ..Output::default()
            },
            paths,
        ))
    }
    async fn execute(&self, name: &str, context: ToolContext, input: Value) -> Result<Value> {
        if name == "wait" {
            return self.wait(context, input).await;
        }
        if name == "jobs" {
            validate_fields(&input, &[])?;
            return self.jobs(&context.session_id);
        }
        // One snapshot controls admission, foreground wait and output capture.
        let config = self.config();
        let starting = name == "exec";
        let cancelling = name == "cancel";
        let fields: &[&str] = match name {
            "exec" => &[
                "command",
                "cwd",
                "workdir",
                "yieldMs",
                "timeoutMs",
                "background",
            ],
            "poll" => &["handle", "cursor", "yieldMs"],
            "cancel" => &["handle"],
            _ => bail!("unknown process tool"),
        };
        validate_fields(&input, fields)?;
        let background = match input.get("background") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| anyhow!("background must be a boolean"))?,
        };
        let sync_ms = config.sync_timeout_secs.saturating_mul(1000);
        let yield_ms = if starting {
            exec_yield_ms(&input, sync_ms)?
        } else {
            unsigned(&input, "yieldMs", Some(0))?
                .unwrap()
                .min(MAX_YIELD_MS)
        };
        let (stdout_cursor, stderr_cursor) = match input.get("cursor") {
            None => (0, 0),
            Some(cursor) => {
                validate_fields(cursor, &["stdout", "stderr"])?;
                (
                    unsigned(cursor, "stdout", Some(0))?.unwrap(),
                    unsigned(cursor, "stderr", Some(0))?.unwrap(),
                )
            }
        };
        let (handle, session) = if starting {
            self.start(&context, &input, &config)?
        } else {
            let session = self.session(string(&input, "handle")?, &context.session_id)?;
            (session.handle.clone(), session)
        };
        if cancelling {
            cancel(&session).await;
        }
        let mut reason = None;
        if starting {
            if background || yield_ms == 0 {
                set_background(&session, None, &self.wake_coordinator())?;
                if !background && input.get("yieldMs").is_none() {
                    reason = Some("sync_timeout");
                }
            } else {
                match await_foreground(
                    &session,
                    yield_ms,
                    &context.cancellation,
                    &self.wake_coordinator(),
                )
                .await?
                {
                    ForegroundOutcome::Finished => {}
                    ForegroundOutcome::Background(value) => reason = Some(value),
                    ForegroundOutcome::Timeout => {
                        reason = if yield_ms == sync_ms {
                            Some("sync_timeout")
                        } else {
                            None
                        };
                        set_background(&session, reason, &self.wake_coordinator())?;
                    }
                }
            }
        } else if yield_ms > 0 && !cancelling {
            await_session(&session, Some(yield_ms), &context.cancellation).await?;
        }
        let value = if name == "poll" && input.get("cursor").is_none() {
            let mut cursors = session
                .read_cursor
                .lock()
                .map_err(|_| anyhow!("process cursor poisoned"))?;
            let (stdout_cursor, stderr_cursor) = *cursors;
            let value = self.snapshot(&handle, &session, stdout_cursor, stderr_cursor, reason)?;
            *cursors = (
                value["cursor"]["stdout"].as_u64().unwrap_or(stdout_cursor),
                value["cursor"]["stderr"].as_u64().unwrap_or(stderr_cursor),
            );
            value
        } else {
            self.snapshot(&handle, &session, stdout_cursor, stderr_cursor, reason)?
        };
        if value["status"] != "running" && value["status"] != "backgrounded" {
            // The terminal status is in this tool result, so the model has it.
            if let Ok(mut state) = session.state.lock() {
                state.delivered = true;
            }
            remove_spool(&session);
        }
        Ok(value)
    }
    fn snapshot(
        &self,
        handle: &str,
        session: &ProcessSession,
        stdout_cursor: u64,
        stderr_cursor: u64,
        reason: Option<&str>,
    ) -> Result<Value> {
        let status = session
            .state
            .lock()
            .map_err(|_| anyhow!("process status poisoned"))?
            .status
            .clone()
            .filter(|_| session.finalized.load(Ordering::Acquire));
        let mut stdout = session
            .stdout
            .lock()
            .map_err(|_| anyhow!("stdout poisoned"))?;
        let mut stderr = session
            .stderr
            .lock()
            .map_err(|_| anyhow!("stderr poisoned"))?;
        let (out, out_capture) = stdout.since(stdout_cursor)?;
        let (err, err_capture) = stderr.since(stderr_cursor)?;
        let mut value = json!({"status":status.as_ref().map_or(if reason.is_some(){"backgrounded"}else{"running"},ProcessStatus::name),"handle":handle,"stdout":out,"stderr":err,"cursor":{"stdout":stdout.total,"stderr":stderr.total},"exitCode":status.as_ref().and_then(|s|s.exit_code),"capture":{"stdout":out_capture,"stderr":err_capture}});
        if let Some(note) = status.as_ref().and_then(|status| status.note.as_deref()) {
            value["note"] = json!(note);
        }
        if status.is_none()
            && let Some(reason) = reason
        {
            value["reason"] = json!(reason);
        }
        if status.is_some()
            && stdout.total.saturating_add(stderr.total) > crate::spill::RESULT_LIMIT_BYTES as u64
            && let Some(spills) = &self.spills
        {
            let mut saved = session
                .full_output
                .lock()
                .map_err(|_| anyhow!("process spill poisoned"))?;
            if saved.is_none() {
                let reference = (|| -> Result<Value> {
                    let out = stdout.full_text()?.ok_or_else(|| anyhow!("stdout exceeded its capture limit or had a capture error; complete output is unavailable"))?;
                    let err = stderr.full_text()?.ok_or_else(|| anyhow!("stderr exceeded its capture limit or had a capture error; complete output is unavailable"))?;
                    let mut full = value.clone();
                    full.as_object_mut().unwrap().remove("stdout");
                    full.as_object_mut().unwrap().remove("stderr");
                    for stream in ["stdout", "stderr"] {
                        full["capture"][stream]["gaps"] = json!([]);
                    }
                    spills.save(
                        &session.owner,
                        &format!("[process result]\n{full}\n[stdout]\n{out}\n[stderr]\n{err}"),
                    )
                })();
                *saved = Some(match reference {
                    Ok(reference) => json!({"fullOutput":reference}),
                    Err(error) => {
                        json!({"fullOutputError":format!("Could not save full output: {error}")})
                    }
                });
            }
            for (key, entry) in saved.as_ref().unwrap().as_object().unwrap() {
                value[key] = entry.clone();
            }
        }
        Ok(with_process_diagnostic(value))
    }
    async fn wait(&self, context: ToolContext, input: Value) -> Result<Value> {
        validate_fields(&input, &["ms", "handle", "timeoutMs"])?;
        if input.get("ms").is_some() {
            ensure!(
                input.get("handle").is_none() && input.get("timeoutMs").is_none(),
                "wait accepts either ms or handle with optional timeoutMs"
            );
            let ms = unsigned(&input, "ms", None)?.unwrap();
            tokio::select! { _ = tokio::time::sleep(Duration::from_millis(ms)) => {}, _ = context.cancellation.cancelled() => bail!("cancelled") }
            return Ok(json!({"status":"completed","ms":ms}));
        }
        let timeout = Some(process_wait_ms(&input)?);
        let session = self.session(string(&input, "handle")?, &context.session_id)?;
        let handle = session.handle.as_str();
        let completed = await_session(&session, timeout, &context.cancellation).await?;
        let status = session
            .state
            .lock()
            .map_err(|_| anyhow!("process status poisoned"))?
            .status
            .clone();
        let note = status.as_ref().and_then(|status| status.note.clone());
        let returns_terminal = completed && status.is_some();
        let mut result = json!({"status":if completed {status.as_ref().map_or("running",ProcessStatus::name)} else {"timeout"},"handle":handle,"exitCode":status.and_then(|s|s.exit_code)});
        if returns_terminal {
            // Returned to the model below, so the exit no longer needs a runtime note.
            if let Ok(mut state) = session.state.lock() {
                state.delivered = true;
            }
        }
        if let Some(note) = note {
            result["note"] = json!(note);
        }
        let mut evidence = self.snapshot(handle, &session, 0, 0, None)?;
        evidence["status"] = result["status"].clone();
        if let Some(diagnostic) = process_diagnostic(&evidence) {
            result["diagnostic"] = json!(diagnostic);
        }
        for key in ["fullOutput", "fullOutputError"] {
            if let Some(value) = evidence.get(key) {
                result[key] = value.clone();
            }
        }
        Ok(result)
    }
    pub async fn cancel_owner(&self, owner: &str) {
        let sessions = self
            .sessions
            .lock()
            .map(|v| {
                v.values()
                    .filter(|s| s.owner == owner)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for session in &sessions {
            session.retired.store(true, Ordering::Release);
        }
        futures_util::future::join_all(sessions.iter().map(|s| cancel(s))).await;
        for session in &sessions {
            remove_spool(session);
        }
        if let Ok(mut registry) = self.sessions.lock() {
            registry.retain(|_, session| session.owner != owner);
        }
        self.wake_coordinator().discard(owner, "process:");
    }
    pub async fn shutdown(&self) {
        // Views only own a workspace binding. Their caller cancels its session
        // through cancel_owner; the root host owns global shutdown.
        if self._owner.is_some() {
            return;
        }
        let sessions = self
            .sessions
            .lock()
            .map(|v| v.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for session in &sessions {
            session.retired.store(true, Ordering::Release);
        }
        futures_util::future::join_all(sessions.iter().map(|s| cancel(s))).await;
        for session in &sessions {
            remove_spool(session);
        }
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.clear();
        }
    }
}
impl Drop for ProcessTools {
    fn drop(&mut self) {
        if self._owner.is_some() {
            return;
        }
        if let Ok(sessions) = self.sessions.lock() {
            for session in sessions.values() {
                session.retired.store(true, Ordering::Release);
                session.tree.kill();
                session.stop.cancel();
                remove_spool(session);
            }
        }
    }
}
/// Enroll before checking completion so a concurrent exit cannot lose its wakeup.
async fn await_session(
    session: &ProcessSession,
    timeout: Option<u64>,
    cancellation: &CancellationToken,
) -> Result<bool> {
    let changed = session.changed.notified();
    tokio::pin!(changed);
    changed.as_mut().enable();
    if session
        .state
        .lock()
        .map_err(|_| anyhow!("process status poisoned"))?
        .status
        .is_some()
        && session.finalized.load(Ordering::Acquire)
    {
        return Ok(true);
    }
    let deadline = async {
        match timeout {
            Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! { biased; _ = cancellation.cancelled() => bail!("cancelled"), _ = &mut changed => Ok(true), _ = deadline => Ok(false) }
}
enum ForegroundOutcome {
    Finished,
    Background(&'static str),
    Timeout,
}
struct ForegroundGuard<'a> {
    session: &'a ProcessSession,
    notices: &'a WakeCoordinator,
}
impl Drop for ForegroundGuard<'_> {
    fn drop(&mut self) {
        let notice = if let Ok(mut state) = self.session.state.lock() {
            state.sync_waiting = false;
            if state.status.is_none() {
                state.backgrounded = true;
            }
            prepare_notice(&mut state)
        } else {
            None
        };
        if let Some(status) = notice {
            publish_notice(self.session, &self.session.handle, &status, self.notices);
        }
    }
}
/// Register both notifications before looking at state, so neither a user
/// background request nor a process exit can be lost during waiter setup.
async fn await_foreground(
    session: &ProcessSession,
    ms: u64,
    cancellation: &CancellationToken,
    notices: &WakeCoordinator,
) -> Result<ForegroundOutcome> {
    let _foreground = ForegroundGuard { session, notices };
    let changed = session.changed.notified();
    let background = session.foreground_changed.notified();
    tokio::pin!(changed, background);
    changed.as_mut().enable();
    background.as_mut().enable();
    {
        let mut state = session
            .state
            .lock()
            .map_err(|_| anyhow!("process status poisoned"))?;
        if state.status.is_some() && session.finalized.load(Ordering::Acquire) {
            return Ok(ForegroundOutcome::Finished);
        }
        if let Some(reason) = state.background_reason {
            return Ok(ForegroundOutcome::Background(reason));
        }
        state.sync_waiting = true;
    }
    tokio::select! {
        biased;
        _=cancellation.cancelled()=>Err(anyhow!("cancelled")),
        _=&mut changed=>Ok(ForegroundOutcome::Finished),
        _=&mut background=>{
            let state=session.state.lock().map_err(|_|anyhow!("process status poisoned"))?;
            Ok(if state.status.is_some(){ForegroundOutcome::Finished}else{ForegroundOutcome::Background(state.background_reason.unwrap_or("user"))})
        },
        _=tokio::time::sleep(Duration::from_millis(ms))=>Ok(ForegroundOutcome::Timeout),
    }
}
fn set_background(
    session: &ProcessSession,
    reason: Option<&'static str>,
    notices: &WakeCoordinator,
) -> Result<()> {
    let mut state = session
        .state
        .lock()
        .map_err(|_| anyhow!("process status poisoned"))?;
    state.backgrounded = true;
    if state.status.is_none() && state.background_reason.is_none() {
        state.background_reason = reason;
    }
    // If exit raced with detachment, it still gets exactly one completion notice.
    let notice = prepare_notice(&mut state);
    drop(state);
    if let Some(status) = notice {
        publish_notice(session, &session.handle, &status, notices);
    }
    session.foreground_changed.notify_waiters();
    Ok(())
}
/// Best-effort startup sweep: removes spool directories whose host is gone
/// (their lock can be taken) and stray files from older layouts.
fn sweep_spools(jobs: &Path) {
    let Ok(entries) = fs::read_dir(jobs) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() || kind.is_symlink() {
            let _ = fs::remove_file(&path);
            continue;
        }
        let stale = match File::options().write(true).open(path.join(".lock")) {
            Ok(lock) => {
                let free = fs2::FileExt::try_lock_exclusive(&lock).is_ok();
                drop(lock);
                free
            }
            Err(_) => true,
        };
        if stale {
            let _ = fs::remove_dir_all(&path);
        }
    }
}
fn prepare_notice(state: &mut ProcessState) -> Option<ProcessStatus> {
    if !state.backgrounded || state.notice_queued {
        return None;
    }
    let status = state.status.clone()?;
    state.notice_queued = true;
    Some(status)
}
// Never acquire the inbox or Store while holding a process-state lock.
// Explicit wait release takes process state, while idle wake admission takes
// the inbox before Harness admission. Publish after releasing process state.
fn publish_notice(
    session: &ProcessSession,
    handle: &str,
    status: &ProcessStatus,
    notices: &WakeCoordinator,
) {
    let stdout = session
        .stdout
        .lock()
        .map(|output| output.total)
        .unwrap_or(0);
    let stderr = session
        .stderr
        .lock()
        .map(|output| output.total)
        .unwrap_or(0);
    let text = format!(
        "Job {handle} {} (exit code {}); stdout {stdout} bytes, stderr {stderr} bytes.",
        status.name(),
        status
            .exit_code
            .map_or_else(|| "none".into(), |code| code.to_string())
    );
    if let Some(store) = &session.store {
        let metadata = json!({"handle":handle,"notice":text}).to_string();
        if let Err(error) = store.store_attachment(
            &session.owner,
            &format!("bubblepaw-process-notice-v1:{handle}"),
            &metadata,
            b"",
        ) {
            let _ = session.diagnostics.stderr(format!(
                "Could not persist known process completion notice: {error}\n"
            ));
        }
    }
    notices.publish_reminder_if(
        &session.owner,
        handle,
        reminder_notices::process_completion(handle, text),
        || !session.retired.load(Ordering::Acquire),
    );
}
fn remove_spool(session: &ProcessSession) {
    if let Ok(mut output) = session.stdout.lock() {
        output.remove_spool();
    }
    if let Ok(mut output) = session.stderr.lock() {
        output.remove_spool();
    }
    for path in &session.spool_paths {
        let _ = fs::remove_file(path);
    }
    if let Some(directory) = session.spool_paths.first().and_then(|path| path.parent()) {
        let _ = fs::remove_dir(directory);
    }
}
/// Each conversation's spool directory. The owner id is kept when it is plain,
/// otherwise hex-encoded.
fn spool_directory(root: &Path, owner: &str) -> PathBuf {
    let owner = if !owner.is_empty()
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        owner.to_owned()
    } else {
        format!(
            "encoded-{}",
            owner
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    };
    root.join(owner)
}
/// Wraps a POSIX script so the shell records its final directory on exit, but
/// only when it differs from `start`; unchanged runs leave no spool file. The
/// trap writes only the marker file, so stdout, stderr and the exit status are
/// unchanged. The trap sits on line 1, so script line numbers in diagnostics stay.
#[cfg(unix)]
fn with_cwd_marker(script: &str, start: &Path, marker: &Path) -> String {
    fn quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
    let report = format!(
        "if [ \"$(pwd -P)\" != {} ]; then pwd -P >{}; fi 2>/dev/null",
        quote(&start.to_string_lossy()),
        quote(&marker.to_string_lossy())
    );
    format!("trap {} EXIT; {script}", quote(&report))
}
/// PowerShell's `finally` runs for explicit exit as well as ordinary completion.
/// Save `$?` before marker housekeeping and restore -Command's 0/1 success exit
/// convention afterward. Explicit exit skips that suffix, preserving its code;
/// terminating errors propagate with their original diagnostic text.
#[cfg(windows)]
fn with_cwd_marker(script: &str, start: &Path, marker: &Path) -> String {
    fn quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', "''"))
    }
    format!(
        r#"try {{
{script}
$__bubblepaw_exec_success = $?
}} finally {{
    try {{
        $__bubblepaw_exec_cwd = $pwd.ProviderPath
        if (![StringComparer]::OrdinalIgnoreCase.Equals($__bubblepaw_exec_cwd, {start})) {{
            [IO.File]::WriteAllText({marker}, $__bubblepaw_exec_cwd + "`n", [Text.UTF8Encoding]::new($false))
        }}
    }} catch {{}}
}}
if (!$__bubblepaw_exec_success) {{ exit 1 }}
exit 0"#,
        start = quote(&windows_path_text(start)),
        marker = quote(&windows_path_text(marker)),
    )
}
#[cfg(not(any(unix, windows)))]
fn with_cwd_marker(script: &str, _start: &Path, _marker: &Path) -> String {
    script.to_owned()
}
/// The reset note when the marker reports a final directory other than the
/// start directory. A missing, empty or partial marker yields no note.
#[cfg(unix)]
fn cwd_note(marker: &Path, start: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let length = fs::metadata(marker).ok()?.len();
    if length == 0 || length > 4096 {
        return None;
    }
    let bytes = fs::read(marker).ok()?;
    // `pwd -P` ends with a newline; anything else is a partial write.
    let reported = bytes.strip_suffix(b"\n")?;
    if reported.is_empty() {
        return None;
    }
    // Both sides are physical paths: `pwd -P` and the canonical start directory.
    if Path::new(std::ffi::OsStr::from_bytes(reported)) == start {
        return None;
    }
    Some(cwd_reset_note(start))
}
#[cfg(windows)]
fn cwd_note(marker: &Path, start: &Path) -> Option<String> {
    let length = fs::metadata(marker).ok()?.len();
    if length == 0 || length > 4096 {
        return None;
    }
    let bytes = fs::read(marker).ok()?;
    let reported = std::str::from_utf8(bytes.strip_suffix(b"\n")?).ok()?;
    if reported.is_empty() || reported.eq_ignore_ascii_case(&windows_path_text(start)) {
        return None;
    }
    Some(cwd_reset_note(start))
}
/// PowerShell's FileSystem provider reports ordinary drive/UNC paths, while
/// workspace canonicalization uses extended-length Windows path prefixes.
#[cfg(windows)]
fn windows_path_text(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    }
}
#[cfg(not(any(unix, windows)))]
fn cwd_note(_marker: &Path, _start: &Path) -> Option<String> {
    None
}
#[cfg(any(unix, windows))]
fn cwd_reset_note(start: &Path) -> String {
    format!(
        "Working directory reset to {}; each exec starts there. Pass `cwd` to run elsewhere.",
        start.display()
    )
}
async fn cancel(session: &ProcessSession) {
    session.stop.cancel();
    let _ = await_session(session, Some(8_000), &CancellationToken::new()).await;
}
async fn drain(mut stream: impl AsyncRead + Unpin, output: Arc<Mutex<Output>>) {
    let mut bytes = [0u8; 8192];
    loop {
        match stream.read(&mut bytes).await {
            Ok(0) => break,
            Ok(n) => {
                if let Ok(mut output) = output.lock() {
                    output.append(&bytes[..n]);
                }
            }
            Err(_) => {
                if let Ok(mut output) = output.lock() {
                    output.error = true;
                }
                break;
            }
        }
    }
}
struct ProcessTool {
    processes: Arc<ProcessTools>,
    name: &'static str,
}
#[async_trait]
impl Tool for ProcessTool {
    fn take_reminders(&self, session_id: &str) -> Vec<bubblepaw_core::Reminder> {
        if self.name == "exec" {
            self.processes.take_reminders(session_id)
        } else {
            Vec::new()
        }
    }
    fn interrupt_wait(&self, session_id: &str) -> usize {
        if self.name == "exec" {
            self.processes.interrupt_waits(session_id)
        } else {
            0
        }
    }
    fn cancel_session(&self, session_id: &str) -> usize {
        if self.name != "exec" {
            return 0;
        }
        let Ok(sessions) = self.processes.sessions.lock() else {
            return 0;
        };
        let mut stopped = 0;
        for session in sessions.values().filter(|s| s.owner == session_id) {
            if session
                .state
                .lock()
                .is_ok_and(|state| state.status.is_none())
                && !session.stop.is_cancelled()
            {
                session.stop.cancel();
                stopped += 1;
            }
            remove_spool(session);
        }
        stopped
    }
    fn definition(&self) -> ToolDefinition {
        let (description, properties, required) = match self.name {
            "exec" => (
                "Execute a command string in a fresh shell with closed stdin and pipes. An experimental command object {parts,values} runs one executable with literal arguments; values must occupy complete unquoted arguments. cwd defaults to workspace. Sync by default, up to exec.sync_timeout_secs (default 300); returns status:backgrounded with reason:sync_timeout without killing. User backgrounding returns reason:user; explicit host wait release retains reason:steer. Ordinary steering does not release foreground waits. background:true returns immediately with a running handle. Explicit yieldMs collects foreground output up to the sync timeout. timeoutMs is a hard deadline that kills. Output uses byte cursors and 4 KiB-per-stream head/tail previews. Completed output over 64 KiB returns a fullOutput absolute text path when its spool is complete, or fullOutputError when capture was incomplete. Read saved output with read line ranges. The process may access files outside workspace.",
                json!({"command":{"oneOf":[{"type":"string"},{"type":"object","properties":{"parts":{"type":"array","items":{"type":"string"}},"values":{"type":"array","items":{"type":"string"}}},"required":["parts","values"],"additionalProperties":false}]},"cwd":{"type":"string"},"workdir":{"type":"string"},"yieldMs":{"type":"integer","minimum":0},"timeoutMs":{"type":"integer","minimum":1},"background":{"type":"boolean"}}),
                vec!["command"],
            ),
            "poll" => (
                "Poll a conversation-owned process. An omitted cursor reads since the session's previous cursorless poll. The first poll starts at byte zero; exec does not consume it. Pass a cursor for an independent reader. Each stream returns at most 4 KiB; retention and reply-budget gaps are reported explicitly. A terminal poll preserves complete output over 64 KiB in fullOutput before reaping the disk spool; later terminal polls reuse the reference. Capture loss produces fullOutputError. Bounded RAM previews remain.",
                json!({"handle":{"type":"string"},"cursor":{"type":"object","properties":{"stdout":{"type":"integer","minimum":0},"stderr":{"type":"integer","minimum":0}},"additionalProperties":false},"yieldMs":{"type":"integer","minimum":0}}),
                vec!["handle"],
            ),
            "cancel" => (
                "Cancel a conversation-owned process and terminate its tree where the OS permits.",
                json!({"handle":{"type":"string"}}),
                vec!["handle"],
            ),
            "wait" => (
                "Wait asynchronously for ms, or for a process handle. Optional timeoutMs returns timeout without cancelling the process. Cancellation always wakes the wait.",
                json!({"ms":{"type":"integer","minimum":0},"handle":{"type":"string"},"timeoutMs":{"type":"integer","minimum":0}}),
                vec![],
            ),
            "jobs" => (
                "List this session's process handles, status, exit code, output byte counts and age. Jobs belong to this host and cannot be recovered after restart.",
                json!({}),
                vec![],
            ),
            _ => unreachable!("registered process capability"),
        };
        ToolDefinition {
            approval: Default::default(),
            name: self.name.into(),
            description: description.into(),
            input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            read_only: matches!(self.name, "wait" | "jobs"),
        }
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        Ok(ToolResult::text(serde_json::to_string(
            &self
                .processes
                .execute(self.name, context, input)
                .await
                .map_err(|error| anyhow!("{} could not run: {error:#}", self.name))?,
        )?))
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        let value = self
            .processes
            .execute(self.name, context.clone(), input)
            .await
            .map_err(|error| anyhow!("{} could not run: {error:#}", self.name))?;
        let starting = self.name == "exec";
        let running = value["status"] == "running" || value["status"] == "backgrounded";
        let content = serde_json::to_string(&value)?;
        if starting && running {
            let handle = string(&value, "handle")?.to_owned();
            let session = self.processes.session(&handle, &context.session_id)?;
            // The background wait ends with its owner: a cancelled session must
            // not leave a waiter holding the effect boundary open.
            let cancellation = context.cancellation.clone();
            tokio::spawn(async move {
                if await_session(&session, None, &cancellation).await.is_ok()
                    && let Err(error) = dispatcher.complete_effect(&context.invocation_id)
                {
                    tracing::warn!(
                        target: "bubblepaw::effects",
                        invocation = %context.invocation_id,
                        %error,
                        "could not complete a finished process effect"
                    );
                }
            });
            Ok(ToolResult::pending(content))
        } else {
            Ok(ToolResult::text(content))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_wait_defaults_to_ten_minutes_and_honors_any_explicit_duration() {
        assert_eq!(process_wait_ms(&json!({})).unwrap(), 600_000);
        for timeout in [0, 5, 900_001, u64::MAX] {
            assert_eq!(
                process_wait_ms(&json!({"timeoutMs":timeout})).unwrap(),
                timeout
            );
        }
    }

    /// Prints the two markers: a client-only value, then HOME (host-only on this test's
    /// host, absent from the snapshot). `unset` means the variable is not in the environment.
    #[cfg(unix)]
    const ENV_SCRIPT: &str = r##"printf '%s|%s' "${CLIENT_ONLY-unset}" "${HOME-unset}""##;

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_sees_exactly_the_inherited_snapshot_and_host_inheritance_returns_on_none() {
        let root = tempfile::tempdir().unwrap();
        let processes = ProcessTools::new(WorkspaceTools::new(root.path()).unwrap());
        let context = ToolContext {
            session_id: "session".into(),
            run_id: String::new(),
            invocation_id: uuid::Uuid::new_v4().to_string(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        };

        processes.set_inherited_env(Some(vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("CLIENT_ONLY".into(), "from-client".into()),
        ]));
        let value = processes
            .execute("exec", context.clone(), json!({"command": ENV_SCRIPT}))
            .await
            .unwrap();
        assert_eq!(
            value["stdout"].as_str(),
            Some("from-client|unset"),
            "{value}"
        );

        // Without a snapshot the command inherits the host environment as before.
        processes.set_inherited_env(None);
        let value = processes
            .execute("exec", context, json!({"command": ENV_SCRIPT}))
            .await
            .unwrap();
        let home = std::env::var("HOME").unwrap_or_else(|_| "unset".into());
        assert_eq!(
            value["stdout"].as_str(),
            Some(format!("unset|{home}").as_str()),
            "{value}"
        );
    }

    #[test]
    fn explicit_exec_windows_are_not_clamped_and_zero_yield_still_wins() {
        assert_eq!(exec_yield_ms(&json!({}), 300_000).unwrap(), 300_000);
        assert_eq!(
            exec_yield_ms(&json!({"timeoutMs":100}), 1000).unwrap(),
            1000
        );
        assert_eq!(
            exec_yield_ms(&json!({"timeoutMs":900_001}), 1000).unwrap(),
            900_001
        );
        assert_eq!(
            exec_yield_ms(&json!({"yieldMs":900_001}), 1000).unwrap(),
            900_001
        );
        assert_eq!(
            exec_yield_ms(&json!({"yieldMs":0,"timeoutMs":900_001}), 1000).unwrap(),
            0
        );
    }
}
