mod cli_store;
mod ephemeral_dir;
use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::Store;
#[cfg(feature = "bench")]
use bubblepaw_runner::{
    chapter_bench_cli, cli_host::episode_model_config, config::selected_main_provider,
    summarization::SummaryConfig,
};
use bubblepaw_runner::{
    cli,
    config::{EffectiveConfig, Override, Settings},
    host_attach::{self, PreparedRequest},
    host_config::{self, ConfigOptions},
    input_files::InputLimits,
    pricing::PriceTable,
    trace::read_report,
    usage::UsageSettings,
    usage_report,
};
#[cfg(test)]
use clap::Parser;
use clap::{CommandFactory, FromArgMatches};
use cli::{
    Cli, Command, DelegationsCommand, LoginProvider, ProviderChoice, QuotaCommand, UsageCommand,
};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};

fn config_overrides(
    cli: &Cli,
    arguments: &[String],
    selected_provider: &str,
) -> Result<Vec<Override>> {
    use toml::Value as T;
    let mut overrides = Vec::new();
    let mut args = arguments.iter().skip(1).peekable();
    while let Some(argument) = args.next() {
        if argument == "--" {
            break;
        }
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
        let mut value = || -> Result<String> {
            inline
                .map(str::to_owned)
                .or_else(|| args.next().cloned())
                .context("Missing CLI value")
        };
        let entry = match flag {
            "--set" => Some(Override::set(&value()?)?),
            "--data-dir" => Some(Override::value("storage.data_dir", T::String(value()?))),
            "--model" => Some(Override::value("model.main", T::String(value()?))),
            "--provider" => Some(Override::value("model.provider", T::String(value()?))),
            "--base-url" => Some(Override::value(
                format!("providers.{selected_provider}.base_url"),
                T::String(value()?),
            )),
            "--auth-file" => Some(Override::value(
                format!("providers.{selected_provider}.auth_file"),
                T::String(value()?),
            )),
            "--context-strategy" => Some(Override::value("context.strategy", T::String(value()?))),
            "--compact-threshold-tokens"
            | "--context-window-tokens"
            | "--episode-retry-base-ms" => {
                let key = match flag {
                    "--compact-threshold-tokens" => "context.compact_threshold_tokens",
                    "--context-window-tokens" => "context.window_tokens",
                    _ => "context.retry_base_ms",
                };
                Some(Override::value(
                    key,
                    T::Integer(value()?.parse().context("Invalid integer CLI setting")?),
                ))
            }
            "--no-auto-summarize" => {
                Some(Override::value("context.auto_summarize", T::Boolean(false)))
            }
            "--web" => Some(Override::value("web.enabled", T::Boolean(true))),
            "--web-search" => Some(Override::value("web.search", T::String(value()?))),
            "--web-expose" => {
                let methods: Vec<String> =
                    value()?.split(',').map(|s| s.trim().to_owned()).collect();
                ensure!(
                    methods
                        .iter()
                        .all(|m| matches!(m.as_str(), "fetch" | "search")),
                    "web-expose accepts fetch and search"
                );
                for method in ["fetch", "search"] {
                    overrides.push(Override::value(
                        format!("web.expose.{method}"),
                        T::Boolean(methods.iter().any(|m| m == method)),
                    ));
                }
                None
            }
            "--read-only" => Some(Override::value("approvals.read_only", T::Boolean(true))),
            "--approve-all" => Some(Override::value(
                "approvals.confirm",
                T::String("allow".into()),
            )),
            "--auto-approve" => {
                let model = inline.map(str::to_owned).or_else(|| {
                    let has_model = args.peek().is_some_and(|next| {
                        !next.starts_with('-')
                            && !matches!(
                                next.as_str(),
                                "run"
                                    | "script"
                                    | "resume"
                                    | "stdio"
                                    | "sessions"
                                    | "history"
                                    | "stats"
                                    | "trace-report"
                                    | "web"
                                    | "config"
                                    | "export"
                                    | "usage"
                                    | "delete"
                                    | "catalog"
                                    | "serving"
                            )
                            && !(cfg!(feature = "bench")
                                && matches!(next.as_str(), "replay" | "chapter-bench"))
                    });
                    has_model.then(|| args.next().unwrap().clone())
                });
                if let Some(model) = model.filter(|model| !model.is_empty()) {
                    overrides.push(decision_override("approver", model));
                }
                Some(Override::value(
                    "approvals.confirm",
                    T::String("auto".into()),
                ))
            }
            "--confirm" | "--confirm-auto" => {
                let mode = if flag == "--confirm" { "ask" } else { "auto" };
                for name in value()?.split(',') {
                    overrides.push(Override::value(
                        format!("approvals.tools.{}", name.trim()),
                        T::String(mode.into()),
                    ));
                }
                None
            }
            "--summary-model" | "--episode-summary-model" => {
                let model = value()?;
                let model = if model.contains('/') {
                    model
                } else {
                    format!("opencode/{model}")
                };
                Some(Override::value(
                    "roles.summary",
                    T::Array(vec![T::String(model), T::String("main".into())]),
                ))
            }
            "--decider" => Some(decision_override("decider", value()?)),
            "--approver" => Some(decision_override("approver", value()?)),
            "--episode-summary-base-url" => Some(Override::value(
                "providers.opencode.base_url",
                T::String(value()?),
            )),
            "--episode-summary-key-env" => Some(Override::value(
                "providers.opencode.key_env",
                T::String(value()?),
            )),
            "--episode-summary-reasoning-fields" => {
                let fields: Value = serde_json::from_str(&value()?).map_err(|_| {
                    anyhow::anyhow!("Provider reasoning fields must be a JSON object")
                })?;
                Some(Override::replace(
                    "providers.opencode.reasoning",
                    T::try_from(fields).map_err(|_| {
                        anyhow::anyhow!("Provider reasoning fields must be a JSON object")
                    })?,
                ))
            }
            "--extensions" => Some(Override::extensions_json(value()?)?),
            // Consume other option values, so their contents cannot become config flags.
            "--config"
            | "--workspace"
            | "--db"
            | "--max-prompt-chars"
            | "--max-attachment-bytes"
            | "--max-attachment-total-bytes"
            | "--attachment-inline-tokens"
            | "--trace-file"
            | "--trace-mode"
            | "--trace-label"
            | "--trace-max-mib"
            | "--session"
            | "--since"
            | "--name"
            | "--chat"
            | "--out"
            | "--gold"
            | "--arm"
            | "--max-cost-usd"
            | "--threshold"
            | "--port"
            | "--assets" => {
                value()?;
                None
            }
            _ => None,
        };
        if let Some(entry) = entry {
            overrides.push(entry);
        }
    }
    let _ = cli; // The parsed CLI already checked flag syntax and value bounds.
    Ok(overrides)
}

fn decision_override(role: &str, model: String) -> Override {
    Override::value(
        format!("roles.{role}"),
        toml::Value::Array(
            [model, "llm/summary".into(), "llm/main".into()]
                .into_iter()
                .map(toml::Value::String)
                .collect(),
        ),
    )
}

/// The same layering as `HostConfig::load`, with the CLI's flags as overrides.
fn load_config(cli: &Cli, arguments: &[String]) -> Result<EffectiveConfig> {
    let load = |provider: &str| -> Result<EffectiveConfig> {
        Ok(host_config::layer(&ConfigOptions {
            workspace: Some(cli.workspace.clone()),
            config_file: cli.config.clone(),
            overrides: config_overrides(cli, arguments, provider)?,
            ..ConfigOptions::default()
        })?)
    };
    let config = load(match cli.provider {
        ProviderChoice::Codex => "codex",
        ProviderChoice::Api => "api",
        ProviderChoice::Demo => "demo",
    })?;
    let config = if cli.base_url.is_some() || cli.auth_file.is_some() {
        let provider = config
            .value("model.provider")
            .and_then(toml::Value::as_str)
            .context("model.provider must be a string")?;
        load(provider)?
    } else {
        config
    };
    Ok(config)
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    use std::io::Write;
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, value)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

fn run_prompt(prompt: Option<&str>, prompt_file: Option<&std::path::Path>) -> Result<String> {
    match (prompt, prompt_file) {
        (Some(prompt), None) => Ok(prompt.to_owned()),
        (None, Some(path)) => std::fs::read_to_string(path)
            .with_context(|| format!("read UTF-8 prompt file {}", path.display())),
        _ => bail!("run requires either a prompt or --prompt-file, but not both"),
    }
}

/// Data directory, legacy database, and whether the data directory is the
/// central default (only that store imports its legacy siblings).
fn storage_paths(cli: &Cli, settings: &Settings) -> Result<(PathBuf, PathBuf, bool)> {
    if let Some(db) = &cli.db {
        let data_dir = Store::legacy_data_dir(db);
        eprintln!(
            "--db is a legacy alias: storage now uses {}; normal startup migrates existing {} once and preserves it",
            data_dir.display(),
            db.display()
        );
        return Ok((data_dir, db.clone(), false));
    }
    let paths = host_config::storage_paths(settings)?;
    Ok((paths.data_dir, paths.legacy_db, paths.central))
}

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    {
        // Heap-pinning bounds future moves, but MSVC debug poll frames can
        // still exceed the Windows entry thread's 1 MiB stack. Match the
        // ordinary Linux main-stack allowance without changing worker tasks.
        match std::thread::Builder::new()
            .name("bubblepaw-runtime".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(run_runtime)
        {
            Ok(thread) => match thread.join() {
                Ok(code) => code,
                Err(panic) => std::panic::resume_unwind(panic),
            },
            Err(error) => {
                eprintln!("Error: cannot start runtime thread: {error}");
                std::process::ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(windows))]
    run_runtime()
}

#[tokio::main]
async fn run_runtime() -> std::process::ExitCode {
    bubblepaw_runner::logging::install();
    // The Windows entry thread has a 1 MiB stack, including debug-mode
    // temporaries created while constructing and polling the CLI future.
    match Box::pin(run_cli()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if error.downcast_ref::<host_attach::RemoteFailure>().is_none() {
                eprintln!("Error: {error:?}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run_cli() -> Result<()> {
    let arguments: Vec<String> = std::env::args().collect();
    let command = Cli::command();
    // Optional values must yield to subcommands, but required global values
    // such as `--web-expose search` must remain values. Keep the original
    // arguments for the ordered configuration overrides below.
    let required_values: std::collections::HashSet<_> = command
        .get_arguments()
        .filter(|arg| {
            arg.get_action().takes_values()
                && arg
                    .get_num_args()
                    .is_none_or(|range| range.min_values() > 0)
        })
        .filter_map(|arg| arg.get_long().map(|name| format!("--{name}")))
        .collect();
    let mut parse_arguments = Vec::with_capacity(arguments.len());
    let mut argument_iter = arguments.iter().peekable();
    while let Some(argument) = argument_iter.next() {
        if required_values.contains(argument)
            && argument_iter
                .peek()
                .is_some_and(|value| !value.starts_with('-'))
        {
            parse_arguments.push(format!("{argument}={}", argument_iter.next().unwrap()));
        } else {
            parse_arguments.push(argument.clone());
        }
    }
    let matches = command.get_matches_from(parse_arguments);
    let cli = Cli::from_arg_matches(&matches)?;
    let mut effective = load_config(&cli, &arguments)?;
    // A delegation picks its model before anything else reads the configuration;
    // the choice is applied as the last CLI override so every role follows it.
    let delegation = if let Command::Delegate {
        assignment, seed, ..
    } = &cli.command
    {
        let text = std::fs::read_to_string(assignment)
            .with_context(|| format!("read assignment {}", assignment.display()))?;
        let assignment = bubblepaw_runner::delegate::assignment::Assignment::parse(&text)?;
        let mut rng = seed.map_or_else(
            bubblepaw_runner::delegate::routing::Rng::from_entropy,
            bubblepaw_runner::delegate::routing::Rng::seeded,
        );
        let route = bubblepaw_runner::delegate::routing::choose(
            &assignment,
            &effective.settings()?.delegate,
            &mut rng,
        )?;
        let mut routed = arguments.clone();
        let at = routed
            .iter()
            .position(|a| a == "--")
            .unwrap_or(routed.len());
        routed.insert(
            at,
            format!(
                "--set=model.main={}",
                toml::Value::String(route.model.clone())
            ),
        );
        // The route override must be layered on the same config, not replace it.
        effective = load_config(&cli, &routed)?;
        Some((assignment, route))
    } else {
        None
    };
    if matches!(cli.command, Command::Config) {
        use std::io::Write;
        std::io::stdout()
            .lock()
            .write_all(effective.render()?.as_bytes())?;
        return Ok(());
    }
    if let Command::TraceReport { path } = &cli.command {
        return print_json(&read_report(path)?);
    }
    let settings = effective.settings()?;
    // Resolve once before creating storage or registering providers. Both inputs
    // follow the same ordinary prompt limits and command handling below.
    let task_prompt = match &cli.command {
        Command::Run {
            prompt,
            prompt_file,
            ..
        } => Some(run_prompt(prompt.as_deref(), prompt_file.as_deref())?),
        _ => None,
    };
    #[cfg(feature = "bench")]
    if let Command::ChapterBench(args) = &cli.command {
        return print_json(&chapter_bench_cli::run(args, &settings).await?);
    }
    #[cfg(feature = "bench")]
    if let Command::Replay {
        chat,
        out,
        threshold,
        dry_run,
        probes,
        checkpoints_at_turns,
        plan_probes,
        probe_plan,
        probe_limit,
        max_cost_usd,
    } = &cli.command
    {
        // Replay consumes recorded assistant turns. An explicitly selected
        // Messages primary still supplies concrete `main` role references;
        // the replay pricing/allow-list boundary validates those references.
        let main = if settings
            .providers
            .get(selected_main_provider(&settings))
            .is_some_and(|provider| provider.kind.as_deref() == Some("anthropic-messages"))
        {
            None
        } else {
            Some("replay/no-main-provider")
        };
        let settings = effective.settings_for_main(main)?;
        let config = episode_model_config(&settings);
        let report = bubblepaw_runner::replay::run_with_context_config(
            bubblepaw_runner::replay::ReplayOptions {
                prices: PriceTable::shared(),
                agents: settings.agents.clone(),
                chat: chat.clone(),
                out: out.clone(),
                threshold: *threshold,
                dry_run: *dry_run,
                context_strategy: settings.context_strategy,
                probes: probes.clone(),
                checkpoints_at_turns: checkpoints_at_turns.clone(),
                probe_plan: probe_plan.clone(),
                plan_probes: *plan_probes,
                capture_turn_frontiers: false,
                probe_limit: *probe_limit,
                max_cost_usd: *max_cost_usd,
            },
            config,
            settings.endpoints.clone(),
            SummaryConfig {
                threshold_cap: settings.compact_threshold_tokens,
                context_window: settings.context_window_tokens,
                verbatim_user_tokens: settings.verbatim_user_tokens,
                lcm: settings.lcm.clone(),
                ..SummaryConfig::default()
            },
        )
        .await?;
        return print_json(
            &serde_json::json!({"report":out.join("report.md"),"status":report["status"],"totals":report["totals"]}),
        );
    }
    let workspace = cli
        .workspace
        .canonicalize()
        .context("workspace must exist")?;
    ensure!(workspace.is_dir(), "workspace must be a directory");
    let usage_settings = UsageSettings::from_effective(&effective)?;
    // Choose isolation before migration, opening, or touching the real store.
    // Declared before all store handles so normal/error returns drop them first.
    let ephemeral = matches!(
        &cli.command,
        Command::Run {
            no_session: true,
            ..
        } | Command::Script {
            no_session: true,
            ..
        }
    )
    .then(ephemeral_dir::EphemeralDir::new)
    .transpose()?;
    let (data_dir, legacy_db, central_store) = if let Some(directory) = &ephemeral {
        (
            directory.path().join("data"),
            directory.path().join("no-legacy.sqlite"),
            false,
        )
    } else {
        storage_paths(&cli, &settings)?
    };
    let data_dir = std::path::absolute(data_dir)?;
    let legacy_db = std::path::absolute(legacy_db)?;
    if let Command::Login { provider, account } | Command::Logout { provider, account } =
        &cli.command
    {
        use bubblepaw_runner::opencode_console;
        let LoginProvider::Opencode = provider;
        let account = account.clone().unwrap_or_else(|| {
            settings
                .providers
                .get("opencode")
                .and_then(|provider| provider.account.clone())
                .unwrap_or_else(|| "opencode".into())
        });
        let path = opencode_console::credential_path(&data_dir, &account)?;
        if matches!(cli.command, Command::Logout { .. }) {
            let existed = opencode_console::delete_credential(&path)?;
            let _ = std::fs::remove_file(path.with_extension("lock"));
            println!(
                "{}",
                if existed {
                    format!("Signed out of OpenCode Console for account {account}.")
                } else {
                    format!("No OpenCode Console login was stored for account {account}.")
                }
            );
            return Ok(());
        }
        let client = opencode_console::ConsoleClient::new()?;
        let credential = client
            .device_login(&mut std::io::stdout(), &opencode_console::tokio_sleeper())
            .await?;
        opencode_console::write_credential(&path, &credential)?;
        println!("Signed in to OpenCode Console for account {account}.");
        return Ok(());
    }
    if let Command::Delegations {
        command: DelegationsCommand::Report,
    } = &cli.command
    {
        let ledger =
            bubblepaw_runner::delegate::ledger::Ledger::open(&data_dir.join("delegations.db"))?;
        return print_json(&ledger.report()?);
    }
    // Export opens only the authoritative chat read-only: no migration, cleanup,
    // workspace binding, credential access or provider registration.
    if let Command::Export { session, out } = &cli.command {
        use std::io::Write;
        if let Some(path) = out {
            ensure!(
                !path.exists(),
                "export output {} must not already exist",
                path.display()
            );
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."));
            let mut writer = tempfile::NamedTempFile::new_in(parent)?;
            Store::export_data_dir_jsonl(&data_dir, session, &mut writer)?;
            writer.flush()?;
            writer.persist_noclobber(path).with_context(|| {
                format!(
                    "create export {} (output must not already exist)",
                    path.display()
                )
            })?;
        } else {
            let mut writer = std::io::stdout().lock();
            Store::export_data_dir_jsonl(&data_dir, session, &mut writer)?;
            writer.flush()?;
        }
        return Ok(());
    }
    // Usage reads the authoritative records directly, like export: no migration,
    // cleanup, workspace binding, credential access or provider registration.
    if let Command::Usage {
        since,
        session,
        command,
    } = &cli.command
    {
        let (since, session, export) = match command {
            Some(UsageCommand::Export { since }) => (since.as_deref(), None, true),
            None => (since.as_deref(), session.as_deref(), false),
        };
        let since = since.map(usage_report::since_rfc3339).transpose()?;
        let rows = usage_report::load(&data_dir, session, since.as_deref())?;
        let prices = PriceTable::shared();
        if export {
            use std::io::Write;
            let mut writer = std::io::stdout().lock();
            usage_report::write_jsonl(&rows, prices, &mut writer)?;
            writer.flush()?;
        } else {
            return print_json(&usage_report::report(
                &rows,
                prices,
                since.as_deref(),
                session,
            ));
        }
        return Ok(());
    }
    if let Command::Quota { command } = &cli.command {
        use bubblepaw_runner::quota_report;
        use std::io::Write;
        let (latest, since) = match command {
            Some(QuotaCommand::Export { since }) => (false, since.as_deref()),
            None => (true, None),
        };
        let since = since.map(usage_report::since_rfc3339).transpose()?;
        let rows = quota_report::load(&data_dir, latest, since.as_deref())?;
        let mut writer = std::io::stdout().lock();
        if latest {
            let balances = Store::read_quota_balances_data_dir(&data_dir)?;
            quota_report::write_latest_with_balances(
                &rows,
                &balances,
                chrono::Utc::now(),
                &mut writer,
            )?;
        } else {
            quota_report::write_jsonl(&rows, &mut writer)?;
        }
        writer.flush()?;
        return Ok(());
    }
    let input_limits = InputLimits {
        prompt_chars: cli.max_prompt_chars,
        max_file_bytes: cli.max_attachment_bytes,
        max_total_bytes: cli.max_attachment_total_bytes,
        inline_tokens: cli.attachment_inline_tokens,
        ..Default::default()
    };
    let one_shot = matches!(
        cli.command,
        Command::Sessions
            | Command::History { .. }
            | Command::Stats
            | Command::Catalog { .. }
            | Command::Serving { .. }
            | Command::Delete { .. }
            | Command::Import { .. }
            | Command::Archive { .. }
            | Command::Unarchive { .. }
            | Command::Search { .. }
    );
    if !one_shot {
        input_limits.validate()?;
        if let Some(prompt) = task_prompt.as_deref() {
            input_limits.validate_prompt(prompt)?;
        }
    }
    let mut election_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let store = loop {
        match host_config::open_store(&data_dir, &legacy_db, &settings) {
            Ok(store) => break store,
            Err(error) if host_attach::is_owned(&error) => {
                if matches!(cli.command, Command::Run { .. } | Command::Delegate { .. }) {
                    match bubblepaw_runner::host_transport::connect(
                        &data_dir,
                        Duration::from_millis(100),
                    )
                    .await
                    {
                        Ok(connection) => {
                            let request = PreparedRequest {
                                cli: cli.clone(),
                                effective: effective.clone(),
                                delegation: delegation.clone(),
                                task_prompt: task_prompt.clone(),
                                workspace: workspace.clone(),
                                data_dir: data_dir.clone(),
                                legacy_db: legacy_db.clone(),
                            };
                            match host_attach::attach(request, connection).await {
                                // A one-shot host imported legacy stores and
                                // stopped without reading the request.
                                Err(error) if error.is::<host_attach::HostRetry>() => {
                                    election_deadline =
                                        tokio::time::Instant::now() + Duration::from_secs(5);
                                    continue;
                                }
                                result => return result,
                            }
                        }
                        Err(error) if tokio::time::Instant::now() >= election_deadline => {
                            return Err(error.context(
                                "could not attach to owning host; request was not submitted",
                            ));
                        }
                        Err(_) => continue,
                    }
                }
                return Err(host_attach::ownership_error(&data_dir, error));
            }
            Err(error) => return Err(error),
        }
    };
    let data_dir = store
        .data_dir()
        .context("host has no data directory")?
        .to_owned();
    // The host imports legacy per-workspace stores once, before serving work.
    // It publishes its endpoint first, so attaching clients wait instead of
    // timing out; a one-shot command releases them to elect again.
    let mut importing = None;
    if central_store && bubblepaw_runner::legacy_import::import_pending(&store)? {
        importing = Some(host_attach::import_legacy_stores(&store, &data_dir).await?);
    }
    if one_shot && let Some(importing) = importing.take() {
        importing.release().await;
    }
    for report in host_config::prepare_store(&store, &data_dir, &settings)? {
        eprintln!("Child worktree {}: {}", report.session_id, report.note);
    }
    if one_shot {
        return cli_store::run(&cli.command, store, &workspace, usage_settings).await;
    }
    let request = PreparedRequest {
        cli,
        effective,
        delegation,
        task_prompt,
        workspace,
        data_dir,
        legacy_db,
    };
    Box::pin(host_attach::serve(request, store, importing)).await
}

#[cfg(test)]
mod role_cli_tests;
