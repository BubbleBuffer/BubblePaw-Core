use bubblepaw_core::ContextStrategy;
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Clone, Serialize, Deserialize)]
#[command(
    name = "bubblepaw",
    version,
    about = "Small Rust agent harness",
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    /// Replace the user and project configuration files with this file.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Override any configuration key with a TOML value (or plain string).
    #[arg(long = "set", global = true)]
    pub settings: Vec<String>,
    /// Workspace directory for the agent's tools; must exist.
    #[arg(long, global = true, default_value = ".")]
    pub workspace: PathBuf,
    /// Legacy single-file archive. Startup migrates it once into a sibling `<stem>.data`
    /// directory and keeps the file. Conflicts with --data-dir.
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,
    /// Directory containing catalog.db, serving.db and chats/<root-session>.db.
    #[arg(long, global = true, conflicts_with = "db")]
    pub data_dir: Option<PathBuf>,
    /// Model provider for the main model: codex (default), api or demo.
    #[arg(long, global = true, value_enum, default_value = "codex")]
    pub provider: ProviderChoice,
    /// Main model id for the selected provider.
    #[arg(long, global = true, default_value = "gpt-6-luna")]
    pub model: String,
    /// Credentials file for the selected provider; overrides its configured auth_file.
    #[arg(long, global = true)]
    pub auth_file: Option<PathBuf>,
    /// Base URL for the selected provider; overrides its configured base_url.
    #[arg(long, global = true)]
    pub base_url: Option<String>,
    /// JSON file of out-of-process provider extensions, `[{"id","command","args","cwd"?}]`.
    /// Their models are addressed as `<id>/<model>`; --provider stays the default.
    #[arg(long, global = true)]
    pub extensions: Option<PathBuf>,
    #[arg(long, global = true)]
    pub read_only: bool,
    /// Enable native web search and page fetching (off by default).
    #[arg(long, global = true)]
    pub web: bool,
    /// Search backend: deepseek, ddg, searxng:<url>, or brave (named API-key environment).
    /// Sets one backend; for an ordered fallback list use
    /// --set 'web.search=["deepseek", "ddg"]' (the configuration default).
    #[arg(long, global = true)]
    pub web_search: Option<String>,
    /// Web methods exposed to this agent: fetch and search.
    #[arg(
        long,
        global = true,
        num_args = 1,
        value_delimiter = ',',
        default_value = "fetch,search"
    )]
    pub web_expose: Vec<String>,
    /// Tools whose calls wait for approval (docs/approvals.md).
    #[arg(long, global = true, value_delimiter = ',')]
    pub confirm: Vec<String>,
    /// Allow tools that declare Confirm without asking.
    #[arg(long, global = true)]
    pub approve_all: bool,
    /// Check Confirm calls with the approver role before asking a person.
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "", conflicts_with = "approve_all")]
    pub auto_approve: Option<String>,
    /// Tools checked by the approver role before asking a person.
    #[arg(long, global = true, value_delimiter = ',')]
    pub confirm_auto: Vec<String>,
    /// Decision model for episode subject splits (adapter/model).
    #[arg(long, global = true)]
    pub decider: Option<String>,
    /// Decision model for automatic approval checks (adapter/model).
    #[arg(long, global = true)]
    pub approver: Option<String>,
    /// Override model capacity when it is not available in local model metadata.
    #[arg(long, global = true)]
    pub context_window_tokens: Option<usize>,
    /// Compact at the smaller of this cap and the model window minus the configured output reserve.
    #[arg(long, global = true, default_value_t = 200_000)]
    pub compact_threshold_tokens: usize,
    #[arg(long, global = true)]
    pub no_auto_summarize: bool,
    /// Context strategy for a new session; an existing session keeps its saved selection.
    #[arg(long, global = true, value_enum)]
    pub context_strategy: Option<ContextStrategyChoice>,
    /// Episode chat-completions base URL; an empty value uses the main provider.
    #[arg(long, global = true)]
    pub episode_summary_base_url: Option<String>,
    #[arg(long = "summary-model", alias = "episode-summary-model", global = true)]
    pub episode_summary_model: Option<String>,
    /// Environment variable containing the episode summary API key.
    #[arg(long, global = true)]
    pub episode_summary_key_env: Option<String>,
    /// JSON object containing chat-completions reasoning fields.
    #[arg(long, global = true)]
    pub episode_summary_reasoning_fields: Option<String>,
    /// Base delay for the episode model calls' exponential retry backoff.
    #[arg(long, global = true)]
    pub episode_retry_base_ms: Option<u64>,
    /// Maximum Unicode characters in ordinary prompt text; large web pastes become files.
    #[arg(long, global = true, default_value_t = 32_000)]
    pub max_prompt_chars: usize,
    /// Maximum bytes in one uploaded input file.
    #[arg(long, global = true, default_value_t = 16 * 1024 * 1024)]
    pub max_attachment_bytes: u64,
    /// Maximum combined attachment bytes in one accepted message.
    #[arg(long, global = true, default_value_t = 32 * 1024 * 1024)]
    pub max_attachment_total_bytes: u64,
    /// Estimated tokens available for the complete attachment envelope, including text.
    #[arg(long, global = true, default_value_t = 10_000)]
    pub attachment_inline_tokens: usize,
    #[arg(long, global = true)]
    pub json: bool,
    /// Opt in to disposable behavior traces, separate from the session archive.
    #[arg(long, global = true)]
    pub trace_file: Option<PathBuf>,
    /// Contents mode captures bounded prompts, code, commands and tool results.
    #[arg(long, global = true, value_enum, default_value = "metadata")]
    pub trace_mode: TraceMode,
    /// Name this build/configuration for later comparisons.
    #[arg(long, global = true)]
    pub trace_label: Option<String>,
    /// Maximum size of each rotated trace file in MiB.
    #[arg(long, global = true, default_value_t = 32)]
    pub trace_max_mib: u64,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, ValueEnum, Serialize, Deserialize)]
pub enum ProviderChoice {
    Codex,
    Api,
    Demo,
}

#[derive(Clone, Copy, ValueEnum, Serialize, Deserialize)]
pub enum LoginProvider {
    Opencode,
}

#[derive(Clone, Copy, ValueEnum, Serialize, Deserialize)]
pub enum TranscriptFormat {
    ClaudeCode,
    Codex,
}

#[derive(Clone, Copy, ValueEnum, Serialize, Deserialize)]
pub enum TraceMode {
    Metadata,
    Contents,
}

#[derive(Clone, Copy, ValueEnum, Serialize, Deserialize)]
pub enum ContextStrategyChoice {
    Lcm,
    Classical,
}

impl From<ContextStrategyChoice> for ContextStrategy {
    fn from(value: ContextStrategyChoice) -> Self {
        match value {
            ContextStrategyChoice::Lcm => Self::Lcm,
            ContextStrategyChoice::Classical => Self::Classical,
        }
    }
}

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Print effective TOML configuration and its sources; never reads credentials.
    Config,
    /// Run a prompt, optionally continuing a saved session.
    Run {
        #[arg(
            required_unless_present = "prompt_file",
            conflicts_with = "prompt_file"
        )]
        prompt: Option<String>,
        /// Read the task from a UTF-8 file, relative to the invoking directory.
        #[arg(long, required_unless_present = "prompt", conflicts_with = "prompt")]
        prompt_file: Option<PathBuf>,
        #[arg(long)]
        session: Option<String>,
        /// Title of this new session.
        #[arg(long, conflicts_with = "session")]
        name: Option<String>,
        /// Use a private temporary store, removed when the process exits.
        #[arg(long, conflicts_with = "session")]
        no_session: bool,
    },
    /// Run a js file as a model-less run and print its result.
    Script {
        file: PathBuf,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, conflicts_with = "session")]
        name: Option<String>,
        #[arg(long, conflicts_with = "session")]
        no_session: bool,
    },
    /// Resume interrupted or pending work explicitly.
    Resume { session: String },
    /// Bidirectional JSON-RPC records over stdin/stdout; no HTTP server.
    Stdio,
    /// List saved sessions without contacting a provider.
    Sessions,
    /// Print the authoritative conversation.
    History { session: String },
    /// Export one session as JSONL, with complete message content and child links.
    Export {
        session: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Request usage and cost by session, model, provider and day, read from retained
    /// records without contacting a provider. `usage export` writes one JSONL line per request.
    Usage {
        /// Only requests at or after this RFC 3339 time.
        #[arg(long)]
        since: Option<String>,
        /// Only this session's requests.
        #[arg(long)]
        session: Option<String>,
        #[command(subcommand)]
        command: Option<UsageCommand>,
    },
    /// Sign in to a provider console that BubblePaw reads quota from. Only
    /// `opencode` (OpenCode Console device login) exists. Tokens are stored in
    /// a user-only file under the data directory and never printed.
    Login {
        #[arg(value_enum, id = "login_provider", value_name = "PROVIDER")]
        provider: LoginProvider,
        /// Quota account; default: the opencode provider entry's account.
        #[arg(long)]
        account: Option<String>,
    },
    /// Delete a stored console login.
    Logout {
        #[arg(value_enum, id = "login_provider", value_name = "PROVIDER")]
        provider: LoginProvider,
        #[arg(long)]
        account: Option<String>,
    },
    /// Latest subscription and rate-limit readings, without contacting providers.
    Quota {
        #[command(subcommand)]
        command: Option<QuotaCommand>,
    },
    /// Import a local coding-agent transcript without contacting a provider.
    Import {
        #[arg(value_enum)]
        format: TranscriptFormat,
        file: PathBuf,
        #[arg(long)]
        title: Option<String>,
        /// Additional native child transcripts; descendants share the root chat file.
        #[arg(long = "child")]
        children: Vec<PathBuf>,
    },
    /// Compress a whole idle chat, including its child sessions.
    Archive { chat: String },
    /// Restore an archived chat to its ordinary SQLite file.
    Unarchive { chat: String },
    /// Search indexed text across chats; fetch only hit messages for snippets.
    Search {
        query: String,
        #[arg(long)]
        chat: Option<String>,
    },
    /// Delete a session and its descendants; deleting a root removes its chat file.
    Delete { session: String },
    /// Rebuild the small session catalog from authoritative chat files.
    Catalog {
        #[command(subcommand)]
        command: CatalogCommand,
    },
    /// Manage disposable operational storage.
    Serving {
        #[command(subcommand)]
        command: ServingCommand,
    },
    /// Show archive size and write counters.
    Stats,
    /// Summarize disposable JSONL traces without contacting a provider.
    TraceReport { path: PathBuf },
    /// Run one delegated assignment (Markdown with TOML front matter): the worker turn,
    /// harness-run checks and reworks, one ledger row per attempt, then a result record.
    Delegate {
        assignment: PathBuf,
        /// Seed the explore draw, for reproducible routing.
        #[arg(long)]
        seed: Option<u64>,
        /// Title of the worker session.
        #[arg(long)]
        name: Option<String>,
    },
    /// Query the delegation outcome ledger (delegations.db beside the store).
    Delegations {
        #[command(subcommand)]
        command: DelegationsCommand,
    },
    #[cfg(feature = "bench")]
    /// Compare chapter selection on recorded chats without executing their tools.
    ChapterBench(crate::chapter_bench_cli::Args),
    #[cfg(feature = "bench")]
    /// Replay a recorded chat into a fresh isolated SQLite archive and report.
    Replay {
        /// Canonical corpus JSON (an object with "messages"); other formats are rejected.
        #[arg(long)]
        chat: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        threshold: Option<usize>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, requires = "plan_probes")]
        probes: Option<PathBuf>,
        /// Probe after the complete typed-user turn K, before user turn K+1.
        #[arg(long, value_delimiter = ',')]
        checkpoints_at_turns: Vec<usize>,
        /// Build both scripted arms once, or derive a smaller existing plan.
        #[arg(long)]
        plan_probes: bool,
        /// Consume the pinned common probe lists; no per-arm list overrides.
        #[arg(long)]
        probe_plan: Option<PathBuf>,
        #[arg(long)]
        probe_limit: Option<usize>,
        #[arg(long)]
        max_cost_usd: Option<f64>,
    },
    /// Serve the local workbench, embedding the harness in this process.
    #[cfg(feature = "web")]
    Web {
        /// Local port to listen on (127.0.0.1); 0 picks a free port.
        #[arg(long, default_value_t = 8788)]
        port: u16,
        /// Built frontend directory containing index.html (required).
        #[arg(long)]
        assets: PathBuf,
    },
}

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum UsageCommand {
    /// One JSONL object per request (schema bubblepaw.usage.v1), ordered by time.
    Export {
        /// Only requests at or after this RFC 3339 time, inclusive.
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum QuotaCommand {
    /// Change-only quota history as bubblepaw.quota.v1 JSONL.
    Export {
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum CatalogCommand {
    /// Rebuild the session catalog from chat files.
    Rebuild,
}

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum DelegationsCommand {
    /// Cost per accepted outcome by category, class, model and route reason.
    Report,
}

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum ServingCommand {
    /// Delete serving.db and its -wal and -shm files, then create an empty one.
    Wipe,
}
