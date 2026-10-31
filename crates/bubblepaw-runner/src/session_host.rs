use crate::cli_host::*;
use bubblepaw_runner::host_api::{
    HostCtx, Profile, ReadScope, approvals::deny_unattended, dispatch,
};
mod cli_run;
pub mod host_context;
mod open;
#[path = "session_prompts.rs"]
mod prompts;
use cli_run::{OneShot, run_session};
use host_context::{HostContextMode, host_context};
use open::{open_session, providers, workspace_roots};

/// Reason recorded on every approval a one-shot run denies without a client.
const UNATTENDED: &str = "no client to approve; use --approve-all";

pub(super) async fn execute(
    request: PreparedRequest,
    store: Store,
    host: HostLifetime,
    output: Output,
    cancellation: CancellationToken,
    mut host_mode: Option<HostContextMode>,
) -> Result<()> {
    let PreparedRequest {
        cli,
        effective,
        delegation,
        task_prompt,
        workspace,
        data_dir,
        ..
    } = request;
    let settings = effective.settings()?;
    let usage_settings = UsageSettings::from_effective(&effective)?;
    let input_limits = InputLimits {
        prompt_chars: cli.max_prompt_chars,
        max_file_bytes: cli.max_attachment_bytes,
        max_total_bytes: cli.max_attachment_total_bytes,
        inline_tokens: cli.attachment_inline_tokens,
        ..Default::default()
    };
    input_limits.validate()?;
    if let Some(prompt) = task_prompt.as_deref() {
        input_limits.validate_prompt(prompt)?;
    }
    let scoped_request = matches!(
        cli.command,
        Command::Run { .. }
            | Command::Delegate { .. }
            | Command::Script { .. }
            | Command::Resume { .. }
    );
    let scope = host.new_scope();
    let _release_scope = scope.release_on_drop();
    if !scoped_request {
        for id in workspace_roots(&store, &workspace).await? {
            scope.adopt_idle_root(&id);
        }
    }
    let existing = match &cli.command {
        Command::Run { session, .. } | Command::Script { session, .. } => session.as_deref(),
        Command::Resume { session } => Some(session.as_str()),
        _ => None,
    };
    if let Some(session) = existing
        && let Some(bound) = store.session_workspace(session)?
    {
        ensure!(
            bound == workspace,
            "Session belongs to another workspace: {}",
            bound.display()
        );
    }
    macro_rules! diagnostic {
        ($($arg:tt)*) => { output.stderr(format!("{}\n", format_args!($($arg)*)))? };
    }
    let web = settings
        .web
        .net_config_with_env(&|name| effective.environment(name))?;
    if effective.source("context.strategy") != Some("default") {
        let strategy = settings.context_strategy;
        if let Some(session) = existing {
            store.ensure_context_strategy(session, strategy)?;
        }
    }
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools_with_exec(
        &mut tools,
        &workspace,
        false,
        settings.exec.clone(),
        &data_dir,
    )?;
    processes.set_inherited_env(Some(effective.runtime_environment()));
    processes.set_diagnostics(output.clone());
    register_history_tools(&mut tools, store.clone())?;
    // Session commands only: the early returns above start no MCP servers.
    // Each request owns its configured servers; their tools are native capabilities.
    let options = crate::live_config::mcp_options(&effective, &data_dir);
    let mcp = Arc::new(McpHost::start_with_options(&settings.mcp, &workspace, options).await);
    for warning in mcp.warnings() {
        diagnostic!("MCP warning: {warning}");
    }
    let artifacts = Arc::new(ArtifactStore::new(&workspace, data_dir.join("artifacts"))?);
    let repl = Arc::new(CodeMode::new(workspace.clone(), artifacts.clone())?);
    let quota_host = host.quota_for(&store, &settings, &effective)?;
    let model_provider = Arc::new(
        LiveProviderConfig::new(effective.clone())?
            .with_image_store(store.clone())
            .with_quota_host(quota_host),
    );
    let settings = model_provider.snapshot().settings.clone();
    let trace = if let Some(path) = &cli.trace_file {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            workspace.join(path)
        };
        ensure!(
            cli.trace_max_mib > 0 && cli.trace_max_mib <= 1024,
            "trace-max-mib must be between 1 and 1024"
        );
        validate_trace_path(&path, &data_dir)?;
        let trace = JsonlTrace::open_with_output(
            TraceConfig {
                path: path.clone(),
                mode: match cli.trace_mode {
                    TraceMode::Metadata => bubblepaw_core::ObservationMode::Metadata,
                    TraceMode::Contents => bubblepaw_core::ObservationMode::Contents,
                },
                label: cli.trace_label.clone().unwrap_or_default(),
                max_file_bytes: cli.trace_max_mib * 1024 * 1024,
                ..TraceConfig::default()
            },
            output.clone(),
        )?;
        trace.runtime_info(json!({
            "package_version":env!("CARGO_PKG_VERSION"),
            "source_fingerprint":crate::source_fingerprint::source_fingerprint(),
            "source_fingerprint_scope":"main,source_fingerprint,root_prompt,host_attach,host_attach_io,host_lifetime,session_host,cli_output,session_scope,host_transport,config,codemode,input_files,agents,agent_handles,episodes,episode_models,episode_fork,history_snapshot,model_roles,registry,live_provider,images,retry,endpoints,decisions,chat,episode_worker,episode_background,summary_prompt_v2,summary_prompt_v3,chapter_prompt_v1,summarization,summary_prompt,runtime_note,command_template,trace,todo,question,process,core_lib,runtime,dispatch,decision,approvals,observation,host_state",
            "model":settings.main_model,
            "provider":selected_main_provider(&settings),
            "platform":std::env::consts::OS,
            "shell":bubblepaw_runner::command_template::shell_name(),
            "read_only":settings.approvals.read_only,
            "input_limits":input_limits
        }));
        diagnostic!(
            "Experimental behavior tracing enabled: {} ({})",
            path.display(),
            match cli.trace_mode {
                TraceMode::Metadata => "metadata",
                TraceMode::Contents => "contents; bounded local prompts and tool contents",
            }
        );
        Some(trace)
    } else {
        None
    };
    let live_config = LiveConfig::new(
        effective.clone(),
        model_provider.clone(),
        mcp.clone(),
        processes.clone(),
        store.clone(),
    )?;
    let settings = model_provider.snapshot().settings.clone();
    let roles = settings.roles.clone();
    let approval = approval_policy(&settings)?;
    // The approver stays lazy, but must also work after ask/allow -> auto edits.
    let episode_config = {
        let mut episode = episode_model_config(&settings);
        episode.roles = roles;
        Some(episode)
    };
    let notes = Arc::new(RuntimeNotes::new(repl.clone(), processes.clone()));
    // Delegate metrics read the existing observation records; a user trace still
    // receives every record.
    let delegate_collector = delegation.is_some().then(|| {
        bubblepaw_runner::delegate::metrics::Collector::new(
            trace.as_ref().map(JsonlTrace::observation),
        )
    });
    let mut config = HarnessConfig {
        model: model_provider.main_model(),
        provider_resolver: Some({
            let live = model_provider.clone();
            Arc::new(move || live.request_binding())
        }),
        execution_mode: settings.agent.execution_mode,
        max_rounds: settings.agent.max_rounds,
        observation: match &delegate_collector {
            Some(collector) => Some(collector.observation()),
            None => trace.as_ref().map(JsonlTrace::observation),
        },
        approval,
        ..HarnessConfig::default()
    };
    {
        let roles = episode_config
            .as_ref()
            .expect("automatic approval settings")
            .roles
            .clone();
        config.auto_approval = Some(AutoApprovalConfig {
            model: roles.approver.models.first().cloned().unwrap_or_default(),
            prompt: approver_prompt(&settings.approvals),
            working_directory: workspace.to_string_lossy().into_owned(),
            resolve: Some({
                let live = model_provider.clone();
                let directory = workspace.to_string_lossy().into_owned();
                Arc::new(move || {
                    let generation = live.snapshot();
                    let role = generation.settings.roles.approver.clone();
                    AutoApprovalConfig {
                        model: role.models.first().cloned().unwrap_or_default(),
                        prompt: approver_prompt(&generation.settings.approvals),
                        working_directory: directory.clone(),
                        provider: Arc::new(RoleDecisionProvider {
                            registry: generation.registry.clone(),
                            role,
                        }),
                        resolve: None,
                    }
                })
            }),
            provider: Arc::new(RoleDecisionProvider {
                registry: model_provider.snapshot().registry.clone(),
                role: roles.approver,
            }),
        });
    }
    let episode_services;
    {
        let generation = model_provider.snapshot();
        let window = generation.context_window(&generation.settings.main_model);
        let summary_config = SummaryConfig {
            context_window: window,
            threshold_cap: settings.compact_threshold_tokens,
            verbatim_user_tokens: settings.verbatim_user_tokens,
            max_request_bytes: config.max_request_bytes,
            lcm: settings.lcm.clone(),
            ..SummaryConfig::default()
        };
        diagnostic!(
            "Context compaction: automatic={}, threshold={} tokens, model window={}; estimates calibrated by per-request usage",
            settings.auto_summarize,
            summary_config.threshold(),
            window
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown; set --context-window-tokens".into())
        );
        let summary = SummarizingContext::new(
            store.clone(),
            model_provider.clone(),
            summary_config,
            Some(notes.clone()),
        )?
        .with_live_config(model_provider.clone());
        let summary = Arc::new(summary.with_surface_refresh(live_config.surface_refresh()));
        let models = Arc::new(
            HttpEpisodeModels::from_live(
                episode_config.expect("episode settings"),
                model_provider.clone(),
                trace.clone(),
            )?
            .with_episode_config(settings.lcm.episodes.clone())?,
        );
        let worker = EpisodeWorker::new_with_config(
            store.clone(),
            models,
            summary.clone(),
            settings.lcm.episodes.clone(),
        )?;
        summary.set_episode_worker(&worker);
        episode_services = Some((summary.clone(), worker));
        config.context = summary;
    }
    config.context = Arc::new(UsageContext::new(store.clone(), config.context.clone()));
    // One-shot runs have no client to answer. Children share the parent's
    // event stream (docs/session-api.md), so one subscription sees every request.
    let one_shot = scoped_request;
    live_config.configure_harness(&mut config);
    prompts::configure(&mut config, &store, &workspace, &data_dir, &settings.agent)?;
    let wake = Arc::new(WakeCoordinator::new(
        settings.agents.auto_wake,
        Duration::from_millis(settings.agents.wake_debounce_ms),
    ));
    let questions = Arc::new(QuestionTools::new(store.clone(), wake.clone(), !one_shot));
    questions.register(&mut tools)?;
    bubblepaw_runner::todo::register(&mut tools, store.clone())?;
    processes.set_wake(wake.clone());
    processes.set_store(store.clone());
    let registry = Arc::new(
        AgentRegistry::default()
            .with_wake(wake)
            .with_scope(scope.clone())
            .with_output(output.clone()),
    );
    let agents = child_agent_host_live(
        store.clone(),
        model_provider.clone(),
        &tools,
        &repl,
        &live_config.child_harness_config(&config),
        &workspace,
        true,
        web.as_ref(),
        settings.agents.clone(),
        registry.clone(),
        processes.clone(),
        model_provider.clone(),
    )?;
    agents.set_questions(questions.clone())?;
    notes.set_agents(&agents);
    if !scoped_request {
        agents.cleanup_workers()?;
    }
    agents.register(&mut tools)?;
    register_agent_handles(&mut tools, agents.clone())?;
    repl.register(&mut tools)?;
    live_config.bind(tools, &agents)?;
    let tools = live_config.registry();
    bubblepaw_runner::execution_mode::validate_surface(&tools, config.execution_mode)?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        model_provider.clone(),
        tools,
        config,
    ));
    host.register(harness.clone());
    // Attachments are one host capability: stdio, the CLI and the web share these files.
    let input_files = Arc::new(
        bubblepaw_runner::input_files::InputFiles::new(&workspace, input_limits)?
            .with_store(store.clone()),
    );
    // Preserve host metadata; config.get overlays accepted live settings.
    let launch = json!({
        "workspace": workspace.to_string_lossy(),
        "model": settings.main_model,
        "provider": selected_main_provider(&settings),
        "read_only": settings.approvals.read_only,
    });
    // One context per host: stdio, the web and the CLI all dispatch through it.
    let mut ctx = HostCtx::new(harness.clone(), store.clone(), workspace.clone());
    ctx.read_scope = ReadScope::Workspace(workspace.clone());
    ctx.scope = scope.clone();
    ctx.context_strategy = settings.context_strategy;
    ctx.storage_policy = Some(settings.root_storage_policy());
    ctx.hold_admission = scoped_request;
    ctx.input_limits = input_limits;
    ctx.usage = usage_settings.clone();
    ctx.config = launch;
    ctx.providers = Some(providers(&effective, &model_provider, &live_config)?);
    ctx.provider_config = Some(model_provider.clone());
    ctx.live_config = Some(live_config.clone());
    ctx.agents = Some(agents.clone());
    ctx.repl = Some(repl.clone());
    ctx.artifacts = Some(artifacts.clone());
    ctx.input_files = Some(input_files.clone());
    ctx.processes = Some(processes.clone());
    ctx.questions = Some(questions.clone());
    // Resolved per trigger, so the episode worker sees the live prompt and tools.
    let weak_harness = Arc::downgrade(&harness);
    let episode_template: EpisodeTemplate = Arc::new(move |session: &str| {
        let harness = weak_harness.upgrade().context("Harness is shutting down")?;
        harness.request_template(session)
    });
    let episode_background = episode_services.map(|(summary, worker)| {
        EpisodeBackground::start_scoped(
            harness.subscribe(),
            store.clone(),
            summary,
            worker,
            episode_template,
            scope.clone(),
        )
    });
    if !scoped_request {
        agents.recover_questions()?;
    }
    let unattended = one_shot
        .then(|| deny_unattended(ctx.clone(), UNATTENDED.to_owned()))
        .transpose()?;
    let resume_quota: bubblepaw_runner::quota_recovery::ResumeQuota = {
        let harness = harness.clone();
        let agents = agents.clone();
        let providers = model_provider.clone();
        let scope = scope.clone();
        Arc::new(move |wait| {
            if harness
                .store()
                .session(&wait.session_id)?
                .parent_session_id
                .is_some()
            {
                agents.resume_quota_wait(wait, approval_policy_for(&providers.hub().approvals())?)
            } else {
                bubblepaw_runner::quota_recovery::root_resume_scoped(harness.clone(), scope.clone())(
                    wait,
                )
            }
        })
    };
    let quota_recovery = if scoped_request {
        None
    } else {
        Some(
            bubblepaw_runner::quota_recovery::QuotaRecovery::start_scoped(
                harness.clone(),
                resume_quota,
                scope.clone(),
            )?,
        )
    };
    // Keep the large command variants out of the setup/cleanup future. This
    // also bounds debug-build stack moves on the Windows entry thread.
    let outcome = Box::pin(async {
        match &cli.command {
            #[cfg(feature = "web")]
            Command::Web { port } => {
                let provider_setup = providers(&effective, &model_provider, &live_config)?;
                let provider_routes =
                    bubblepaw_runner::web_providers::router(provider_setup.clone());
                let listener =
                    tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port)).await?;
                let address = listener.local_addr()?;
                diagnostic!("BubblePaw host: http://{address}");
                let generation = model_provider.snapshot();
                let settings = generation.settings.clone();
                let config = bubblepaw_runner::web::WebConfig {
                    workspace: workspace.to_string_lossy().into_owned(),
                    model: settings.main_model.clone(),
                    provider: selected_main_provider(&settings).to_owned(),
                    read_only: settings.approvals.read_only,
                };
                bubblepaw_runner::web::serve_with_services(
                    listener,
                    harness.clone(),
                    store.clone(),
                    config,
                    bubblepaw_runner::web::WebServices {
                        read_scope: None,
                        session_scope: Some(scope.clone()),
                        storage_policy: Some(settings.root_storage_policy()),
                        provider_setup: Some(provider_setup),
                        provider_routes: Some(provider_routes),
                        providers: Some(model_provider.clone()),
                        live_config: Some(live_config.clone()),
                        context_strategy: settings.context_strategy,
                        agents: Some(agents.clone()),
                        repl: Some(repl.clone()),
                        processes: Some(processes.clone()),
                        questions: Some(questions.clone()),
                        artifacts: Some(artifacts.clone()),
                        input_files: Some(input_files.clone()),
                        codex_models_cache: effective.codex_models_cache(),
                    },
                    {
                        let cancellation = cancellation.clone();
                        async move {
                            tokio::select! {
                                _ = cancellation.cancelled() => {},
                                _ = tokio::signal::ctrl_c() => {},
                            }
                        }
                    },
                )
                .await
            }
            Command::Stdio => host_context(ctx.clone(), host_mode.take(), &cancellation).await,
            Command::Run { session, name, .. } => {
                let prompt = task_prompt
                    .as_deref()
                    .expect("run prompt resolved before startup");
                let direct =
                    dispatch(&ctx, Profile::Full, "host.command", json!({"text": prompt})).await?;
                if !direct.is_null() && direct["needs_session"] != true {
                    // /reload needs no session, so none is created for it.
                    output.json(&direct)
                } else {
                    let id = open_session(&ctx, session.as_deref(), name.as_deref(), "New session")
                        .await?;
                    if direct.is_null() {
                        let request = OneShot::Prompt(prompt);
                        run_session(
                            &ctx,
                            &id,
                            request,
                            cli.json,
                            output.clone(),
                            cancellation.clone(),
                        )
                        .await
                    } else {
                        let params = json!({"session_id": id, "text": prompt});
                        let result = dispatch(&ctx, Profile::Full, "host.command", params).await?;
                        output.json(&result)
                    }
                }
            }
            Command::Delegate { name, .. } => {
                let (assignment, route) = delegation
                    .clone()
                    .expect("delegation resolved before startup");
                let mut ledger = bubblepaw_runner::delegate::ledger::Ledger::open(
                    &data_dir.join("delegations.db"),
                )?;
                let json_output = cli.json;
                let events: Option<bubblepaw_runner::delegate::EventSink> =
                    json_output.then(|| -> bubblepaw_runner::delegate::EventSink {
                        let output = output.clone();
                        Arc::new(move |event| {
                            let _ = output.event(event, true);
                        })
                    });
                let title = session_title(name.as_deref(), "Delegation")?;
                let result = bubblepaw_runner::delegate::run_controlled(
                    harness.clone(),
                    &store,
                    &mut ledger,
                    bubblepaw_runner::delegate::DelegateRun {
                        assignment,
                        route,
                        workspace: workspace.clone(),
                        title,
                        strategy: settings.context_strategy,
                        events,
                        collector: delegate_collector.clone(),
                    },
                    bubblepaw_runner::delegate::DelegateControl {
                        cancellation: cancellation.clone(),
                        on_session: Some({
                            let scope = scope.clone();
                            Arc::new(move |session| scope.set_root(session))
                        }),
                        storage_policy: Some(settings.root_storage_policy()),
                        environment: Some(effective.runtime_environment()),
                    },
                )
                .await?;
                output.json(&result)?;
                if result.status == "failed" {
                    bail!("delegation failed; ledger id {}", result.ledger_id);
                }
                Ok(())
            }
            Command::Script {
                file,
                session,
                name,
                ..
            } => {
                let body = std::fs::read_to_string(file)
                    .with_context(|| format!("read {}", file.display()))?;
                let id = open_session(&ctx, session.as_deref(), name.as_deref(), "Script").await?;
                let label = format!("[script] {}", file.display());
                let request = OneShot::Script {
                    label: &label,
                    body,
                };
                run_session(
                    &ctx,
                    &id,
                    request,
                    cli.json,
                    output.clone(),
                    cancellation.clone(),
                )
                .await
            }
            Command::Resume { session } => {
                run_session(
                    &ctx,
                    session,
                    OneShot::Resume,
                    cli.json,
                    output.clone(),
                    cancellation.clone(),
                )
                .await
            }
            _ => unreachable!(),
        }
    })
    .await;
    if let Some(recovery) = quota_recovery {
        recovery.shutdown().await;
    }
    // This request may own children or stdio-started runs, but not another
    // request's sessions. Shared runtime shutdown belongs to HostLifetime.
    for session in store
        .list_sessions()?
        .into_iter()
        .filter(|_| !scoped_request)
    {
        if scope.hold_owned_for_cleanup(&store, &session.id)? {
            let cancelled = dispatch(
                &ctx,
                Profile::Full,
                "run.cancel",
                json!({"session_id": session.id}),
            )
            .await;
            if cancelled.is_ok_and(|reply| reply["requested"] == true) {
                let waited = dispatch(
                    &ctx,
                    Profile::Full,
                    "run.wait",
                    json!({"session_id": session.id}),
                );
                let _ = tokio::time::timeout(Duration::from_secs(5), waited).await;
            }
        }
    }
    let children_stopped = agents.shutdown().await;
    if let Some(background) = episode_background {
        background
            .shutdown(if one_shot && !cancellation.is_cancelled() {
                EpisodeBackground::ONE_SHOT_DRAIN
            } else {
                EpisodeBackground::HOST_DRAIN
            })
            .await;
    }
    repl.shutdown().await;
    processes.shutdown().await;
    mcp.shutdown(Duration::from_secs(2)).await;
    scope.release();
    if let Some(task) = unattended {
        task.abort();
        let _ = task.await;
    }
    if let Some(trace) = &trace {
        if let Err(error) = trace.finish(Duration::from_secs(5)) {
            diagnostic!("Trace flush incomplete: {error}");
        }
        diagnostic!("Trace status: {}", serde_json::to_string(&trace.status())?);
    }
    outcome?;
    children_stopped?;
    Ok(())
}
