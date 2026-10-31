//! Host-owned config refresh. Disk edits are checked once at each turn boundary.
use crate::{
    agents::AgentTools,
    config::{ApprovalSettings, EffectiveConfig, Settings, SkillsConfig},
    memory_files::MemoryFiles,
    net::{NetClient, register_sources},
    provider::LiveProviderConfig,
    skills::Skills,
    tools::ProcessTools,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, CancellationToken, Store, ToolRegistry, TurnStart,
};
use bubblepaw_mcp::McpHost;
use serde::Serialize;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock, Weak},
    time::SystemTime,
};

mod diff;
mod reload_tool;
use diff::{add_mcp_diff, config_diff};
mod turn_start;
pub(crate) use reload_tool::register_reload_tool;
use turn_start::ConfigTurnStart;

#[derive(Clone, Debug, Serialize)]
pub struct ReloadReport {
    pub applied: bool,
    pub changed: Vec<String>,
    pub errors: Vec<String>,
    pub restart_needed: bool,
    /// The changed keys behind `restart_needed`, from the section schema.
    pub restart_keys: Vec<String>,
    pub active_main: String,
}
impl ReloadReport {
    pub fn summary(&self) -> String {
        let mut parts = self.changed.clone();
        parts.extend(self.errors.iter().map(|error| format!("! {error}")));
        if parts.is_empty() {
            return "Config unchanged".into();
        }
        let mut text = format!("Config changed: {}", parts.join("; "));
        // A notice is a small diff, never an effective config dump.
        if text.len() > 4096 {
            let mut end = 4050;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("; further changes omitted (see reload result)");
        }
        text
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub(crate) path: PathBuf,
    modified: Option<SystemTime>,
    pub(crate) size: Option<u64>,
}
struct State {
    stamps: Vec<FileStamp>,
    accepted: EffectiveConfig,
    notices: Vec<String>,
    consumed: HashMap<String, usize>,
    last_error: Option<String>,
    last_warning: Option<String>,
}

#[path = "live_context_surface.rs"]
mod context_surface;
#[path = "live_reminders.rs"]
mod reminder_notices;

pub struct LiveConfig {
    memory: MemoryFiles,
    skills: Skills,
    providers: Arc<LiveProviderConfig>,
    mcp: Arc<McpHost>,
    processes: Arc<ProcessTools>,
    store: Store,
    base: RwLock<ToolRegistry>,
    tools: RwLock<ToolRegistry>,
    removed: Mutex<HashMap<String, String>>,
    web_client: Mutex<Option<NetClient>>,
    agents: Mutex<Weak<AgentTools>>,
    state: Mutex<State>,
    updates: tokio::sync::Mutex<()>,
}
impl LiveConfig {
    pub fn new(
        effective: EffectiveConfig,
        providers: Arc<LiveProviderConfig>,
        mcp: Arc<McpHost>,
        processes: Arc<ProcessTools>,
        store: Store,
    ) -> Result<Arc<Self>> {
        let stamps = stamps(&effective)?;
        // The MCP host reconciles to the section the hub publishes.
        mcp.follow(providers.hub().mcp());
        processes.follow(providers.hub().exec());
        let options = providers.load_options();
        let memory = MemoryFiles::new(
            store.clone(),
            options.workspace.clone(),
            options.home.as_ref().map(|home| home.join(".bubblepaw")),
            effective.settings()?.memory_files,
        );
        let skills = Skills::new(
            store.clone(),
            options.workspace.clone(),
            options
                .home
                .as_ref()
                .map(|home| home.join(".config/bubblepaw/skills")),
        );
        Ok(Arc::new(Self {
            memory,
            skills,
            providers,
            mcp,
            processes,
            store,
            base: RwLock::default(),
            tools: RwLock::default(),
            removed: Mutex::default(),
            web_client: Mutex::default(),
            agents: Mutex::new(Weak::new()),
            state: Mutex::new(State {
                stamps,
                accepted: effective,
                notices: Vec::new(),
                consumed: HashMap::new(),
                last_error: None,
                last_warning: None,
            }),
            updates: tokio::sync::Mutex::new(()),
        }))
    }
    /// Install the ordinary host tools once. MCP and web capabilities are rebuilt
    /// separately, and child resolvers hold only a weak link to this host.
    pub fn bind(self: &Arc<Self>, mut base: ToolRegistry, agents: &Arc<AgentTools>) -> Result<()> {
        register_reload_tool(&mut base, Arc::downgrade(self))?;
        agents.follow(self.providers.hub().agents());
        *self.agents.lock().unwrap() = Arc::downgrade(agents);
        *self.base.write().unwrap() = base;
        self.publish_tools()?;
        Ok(())
    }
    /// The last accepted config: what the running host holds for live keys and
    /// what the file asks for restart-only ones.
    pub fn accepted(&self) -> EffectiveConfig {
        self.state.lock().unwrap().accepted.clone()
    }
    pub fn registry(&self) -> ToolRegistry {
        self.tools.read().unwrap().clone()
    }
    pub fn configure_harness(self: &Arc<Self>, config: &mut bubblepaw_core::HarnessConfig) {
        let weak = Arc::downgrade(self);
        config.system_prompt_resolver = Some(Arc::new(move |session, base| match weak.upgrade() {
            Some(live) => {
                if !live.skills.initialized(session) {
                    live.check_skills(session, &live.providers.hub().skills())?;
                }
                let base = live.system_prompt_base(session, base)?;
                let memory = live.memory.system_prompt(session, &base)?;
                let prompt = live.skills.system_prompt(session, &memory);
                live.bind_context_prompt(session, &base, &prompt)?;
                Ok(prompt)
            }
            None => Ok(base.to_owned()),
        }));
        let weak = Arc::downgrade(self);
        config.registry_resolver = Some(Arc::new(move || {
            weak.upgrade()
                .map(|live| live.registry())
                .unwrap_or_default()
        }));
        config.turn_start = Some(Arc::new(ConfigTurnStart(Arc::downgrade(self))));
        let weak = Arc::downgrade(self);
        config.approval_resolver = Some(Arc::new(move |_| {
            weak.upgrade()
                .map(|live| live.approval())
                .unwrap_or_default()
        }));
        // A session's exec tool runs in its worktree, or in the host workspace
        // when it has none; the hardline workspace checks follow that directory.
        let store = self.store.clone();
        config.workspace_resolver = Some(Arc::new(move |session| {
            let host = store.session_workspace(session).ok().flatten()?;
            crate::worktrees::workspace(&store, session, &host).ok()
        }));
    }
    pub fn approval(&self) -> ApprovalPolicy {
        approval_policy_for(&self.providers.hub().approvals())
            .expect("published approval config is validated")
    }
    /// Shared with provider setup writes so a file save cannot overtake a reload.
    pub async fn lock_update(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.updates.lock().await
    }
    pub async fn reload(&self, server: Option<&str>) -> ReloadReport {
        let _guard = self.lock_update().await;
        self.check_locked(true, server).await
    }
    pub async fn check(&self) -> ReloadReport {
        let _guard = self.lock_update().await;
        self.check_locked(false, None).await
    }
    pub async fn check_locked(&self, force: bool, server: Option<&str>) -> ReloadReport {
        let mut report = self.check_locked_inner(force, server).await;
        if report.changed.iter().any(|c| c == "providers updated") {
            report.changed.push(format!("DIAGHISTORY {}", diag_history().lock().unwrap().join(" || ")));
        }
        report
    }
    async fn check_locked_inner(&self, force: bool, server: Option<&str>) -> ReloadReport {
        let old = self.state.lock().unwrap().accepted.clone();
        let mut report = ReloadReport {
            applied: true,
            changed: Vec::new(),
            errors: Vec::new(),
            restart_needed: old.restart_needed(self.providers.startup_config()),
            restart_keys: old.restart_only_keys(self.providers.startup_config()),
            active_main: self.providers.main_model(),
        };
        let current_stamps = match stamps(&old) {
            Ok(stamps) => stamps,
            Err(error) => return self.failed(report, error),
        };
        let disk_changed = current_stamps != self.state.lock().unwrap().stamps;
        diag_note(format!(
            "check force={force} disk_changed={disk_changed} bt={}",
            diag_bt()
        ));
        if force || disk_changed {
            let candidate = (|| {
                let candidate = self.providers.startup_config().reload()?;
                let prepared = self.providers.prepare(candidate.clone())?;
                let settings = candidate.settings()?;
                approval_policy(&settings)?;
                settings.mcp.validate()?;
                if let Some(name) = server {
                    anyhow::ensure!(
                        settings.mcp.servers.get(name).is_some_and(|c| c.enabled),
                        "MCP server {name} is not configured and enabled"
                    );
                }
                Ok::<_, anyhow::Error>((candidate, prepared))
            })();
            let (candidate, prepared) = match candidate {
                Ok(candidate) => candidate,
                Err(error) => {
                    // Remember rejected versions, but never acknowledge an edit
                    // before an awaited reload can be cancelled.
                    self.state.lock().unwrap().stamps = current_stamps;
                    return self.failed(report, error);
                }
            };
            // Publish the sections first (the owners validated them above); the
            // MCP host then reconciles to its section.
            if !self
                .providers
                .snapshot()
                .effective
                .same_provider_config(&candidate)
            {
                let _update = self.providers.lock_update().expect("provider update lock");
                self.providers.apply_prepared(prepared);
            }
            let mcp = match self.mcp.apply(server).await {
                Ok(diff) => diff,
                Err(error) => {
                    self.state.lock().unwrap().stamps = current_stamps;
                    return self.failed(report, error);
                }
            };
            // No await between lifecycle publication, registry publication,
            // notice recording and acknowledgement of the observed file stamp.
            report.changed = config_diff(&old, &candidate);
            add_mcp_diff(&mut report, &mcp);
            report.restart_keys = candidate.restart_only_keys(self.providers.startup_config());
            report.restart_needed = !report.restart_keys.is_empty();
            if let Some(agents) = self.agents.lock().unwrap().upgrade() {
                agents.reconcile();
            }
            {
                let mut state = self.state.lock().unwrap();
                state.accepted = candidate;
                state.stamps = current_stamps;
                state.last_error = None;
            }
            if let Err(error) = self.publish_and_notice(&mut report) {
                return self.failed(report, error);
            }
        }
        // A cancelled notified-list fetch keeps its dirty flag armed. A config
        // already applied above has its registry and pending notice committed.
        let mcp = self.mcp.refresh_tools().await;
        if !mcp.is_empty() {
            let mut refresh = ReloadReport {
                applied: true,
                changed: Vec::new(),
                errors: Vec::new(),
                restart_needed: report.restart_needed,
                restart_keys: report.restart_keys.clone(),
                active_main: self.providers.main_model(),
            };
            add_mcp_diff(&mut refresh, &mcp);
            if let Err(error) = self.publish_and_notice(&mut refresh) {
                refresh = self.failed(refresh, error);
            }
            report.applied &= refresh.applied;
            report.changed.extend(refresh.changed);
            report.errors.extend(refresh.errors);
        }
        report.active_main = self.providers.main_model();
        report
    }
    fn publish_and_notice(&self, report: &mut ReloadReport) -> Result<()> {
        if !report.changed.is_empty() {
            self.publish_tools()?;
        }
        report.active_main = self.providers.main_model();
        let mut state = self.state.lock().unwrap();
        let warning = (!report.errors.is_empty()).then(|| report.errors.join("; "));
        if !report.changed.is_empty() || warning.is_some() && warning != state.last_warning {
            state.notices.push(report.summary());
        }
        state.last_warning = warning;
        Ok(())
    }
    fn failed(&self, mut report: ReloadReport, error: anyhow::Error) -> ReloadReport {
        report.applied = false;
        let error = error.to_string();
        report.errors.push(error.clone());
        let mut state = self.state.lock().unwrap();
        if state.last_error.as_ref() != Some(&error) {
            state.notices.push(report.summary());
            state.last_error = Some(error);
        }
        report
    }
    fn publish_tools(&self) -> Result<()> {
        let mut tools = self.base.read().unwrap().clone();
        let settings = self.providers.snapshot();
        if let Some(agents) = self.agents.lock().unwrap().upgrade() {
            // Profile availability changes the input schema as well as its
            // description. Compile validators from the new definition.
            for name in ["agent", "agent_send", "agent_status"] {
                tools.remove(name, "refreshing agent settings");
            }
            agents.register(&mut tools)?;
            tools.show_always(["agent"]);
        }
        if let Some(web) = self
            .providers
            .hub()
            .web()
            .net_config_with_env(&|name| settings.effective.environment(name))?
        {
            let mut slot = self.web_client.lock().unwrap();
            if slot.is_none() {
                *slot = Some(NetClient::with_store_live(
                    &web,
                    &self.store,
                    self.providers.clone(),
                )?);
            }
            register_sources(
                &mut tools,
                slot.as_ref().expect("web client").clone(),
                web.expose.fetch,
                web.expose.search,
            )?;
        }
        self.mcp.register_tools(&mut tools)?;
        if self.providers.hub().approvals().read_only {
            let mut filtered = tools.read_only();
            // Read-only workspace access still permits child orchestration and
            // cancellation, as at startup. Child tool views inherit the filter.
            for name in ["agent", "agent_send", "cancel"] {
                if let Some(tool) = tools.get_host(name) {
                    filtered.register_host(tool)?;
                    if tools.is_shown_always(name) {
                        filtered.show_always([name]);
                    }
                }
            }
            tools = filtered;
        }
        let previous = self.tools.read().unwrap().clone();
        let mut removed = self.removed.lock().unwrap();
        for definition in previous.capabilities() {
            if tools.get(&definition.name).is_none() {
                removed.insert(
                    definition.name,
                    "removed by live configuration or the server's current tool list".into(),
                );
            }
        }
        removed.retain(|name, _| tools.get(name).is_none());
        for (name, reason) in removed.iter() {
            tools.remove(name, reason);
        }
        *self.tools.write().unwrap() = tools.with_generation(previous.generation() + 1);
        Ok(())
    }
    fn check_skills(&self, session: &str, settings: &SkillsConfig) -> Result<()> {
        self.skills
            .check(session, settings.enabled, &settings.dirs)?;
        self.processes
            .set_skill_read_roots(session, self.skills.read_roots(session))
    }
}

fn stamps(config: &EffectiveConfig) -> Result<Vec<FileStamp>> {
    file_stamps(config.config_paths())
}

pub(crate) fn file_stamps(paths: Vec<PathBuf>) -> Result<Vec<FileStamp>> {
    paths
        .into_iter()
        .map(|path| match std::fs::metadata(&path) {
            Ok(metadata) => Ok(FileStamp {
                path,
                modified: metadata.modified().ok(),
                size: Some(metadata.len()),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileStamp {
                path,
                modified: None,
                size: None,
            }),
            Err(_) => Err(anyhow!("Cannot stat file {}", path.display())),
        })
        .collect()
}

pub fn approval_policy(settings: &Settings) -> Result<ApprovalPolicy> {
    approval_policy_for(&settings.approvals)
}
pub(crate) fn approval_policy_for(approvals: &ApprovalSettings) -> Result<ApprovalPolicy> {
    let mode = |value: &str| match value {
        "ask" => Ok(ApprovalMode::Ask),
        "auto" => Ok(ApprovalMode::Auto),
        "allow" => Ok(ApprovalMode::Allow),
        "deny" => Ok(ApprovalMode::Deny),
        _ => Err(anyhow!("Invalid approval mode")),
    };
    Ok(ApprovalPolicy {
        confirm: mode(&approvals.confirm)?,
        tools: approvals
            .tools
            .iter()
            .map(|(name, value)| Ok((name.clone(), mode(value)?)))
            .collect::<Result<_>>()?,
        deny: approvals.deny.clone(),
        timeout_secs: None,
        hardline: approvals.hardline,
    })
}
/// Recognize only an exact host command; ordinary prompts keep their contents.
pub fn reload_command(text: &str) -> Option<Option<&str>> {
    let mut words = text.split_whitespace();
    if words.next()? != "/reload" {
        return None;
    }
    let server = words.next();
    if words.next().is_some() {
        return None;
    }
    Some(server)
}

pub(crate) fn diag_history() -> &'static Mutex<Vec<String>> {
    static H: std::sync::OnceLock<Mutex<Vec<String>>> = std::sync::OnceLock::new();
    H.get_or_init(Default::default)
}
pub(crate) fn diag_note(text: String) {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let ms = T0.get_or_init(std::time::Instant::now).elapsed().as_millis();
    diag_history().lock().unwrap().push(format!("[{ms}ms] {text}"));
}
pub(crate) fn diag_bt() -> String {
    let bt = std::backtrace::Backtrace::force_capture().to_string();
    let mut out = Vec::new();
    for line in bt.lines() {
        let line = line.trim();
        if line.contains("bubblepaw_") && !line.contains("diag_") && !line.contains("check_locked") {
            if let Some((_, name)) = line.split_once(": ") {
                out.push(name.trim().chars().take(70).collect::<String>());
            }
        }
        if out.len() >= 5 { break; }
    }
    out.join(" < ")
}
