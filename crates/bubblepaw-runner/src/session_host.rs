use super::*;
use crate::stdio_parity::providers;
mod open;
#[path = "session_prompts.rs"]
mod prompts;
use open::{open_or_create, workspace_roots};

pub(super) async fn execute(
    request: PreparedRequest,
    store: Store,
    host: HostLifetime,
    output: Output,
    cancellation: CancellationToken,
) -> Result<()> {
    let PreparedRequest {
        cli,
        effective,
        delegation,
        task_prompt,
        workspace,
        data_dir,
        legacy_db,
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
    let mcp = Arc::new(
        McpHost::start_with_client_env(
            &settings.mcp,
            &workspace,
            bubblepaw_mcp::McpClientEnv::new(effective.mcp_environment()),
        )
        .await,
    );
    for warning in mcp.warnings() {
        diagnostic!("MCP warning: {warning}");
    }
    let artifacts = Arc::new(ArtifactStore::new(&workspace, data_dir.join("artifacts"))?);
    let repl = Arc::new(CodeMode::new(workspace.clone(), artifacts.clone())?);
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
        validate_trace_path(&path, &data_dir, &legacy_db)?;
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
            "model":cli.model,
            "provider":selected_main_provider(&settings),
            "platform":std::env::consts::OS,
            "shell":bubblepaw_runner::command_template::shell_name(),
            "read_only":cli.read_only,
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
    let quota_host = host.quota_for(&store, &settings, &effective)?;
    let model_provider = Arc::new(
        LiveProviderConfig::new(effective.clone())?
            .with_image_store(store.clone())
            .with_quota_host(quota_host),
    );
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
        ensure!(
            cli.context_window_tokens != Some(0),
            "context-window-tokens must be positive"
        );
        let cache = effective.codex_models_cache();
        let window = cli.context_window_tokens.or_else(|| {
            if selected_main_provider(&settings) == "codex" {
                cache
                    .as_ref()
                    .and_then(|p| cached_context_window(p, &cli.model))
            } else {
                None
            }
        });
        let summary_config = SummaryConfig {
            context_window: window,
            threshold_cap: cli.compact_threshold_tokens,
            verbatim_user_tokens: settings.verbatim_user_tokens,
            max_request_bytes: config.max_request_bytes,
            lcm: settings.lcm.clone(),
            ..SummaryConfig::default()
        };
        diagnostic!(
            "Context compaction: automatic={}, threshold={} tokens, model window={}; estimates calibrated by per-request usage",
            !cli.no_auto_summarize,
            summary_config.threshold(),
            window
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown; set --context-window-tokens".into())
        );
        let mut summary = SummarizingContext::new(
            store.clone(),
            model_provider.clone(),
            summary_config,
            Some(notes.clone()),
        )?
        .with_auto_summarize(!cli.no_auto_summarize);
        if cli.context_window_tokens.is_none()
            && selected_main_provider(&settings) == "codex"
            && let Some(cache) = cache
        {
            summary = summary.with_model_metadata(cli.model.clone(), cache);
        }
        let summary = Arc::new(summary);
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
        &config,
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
    // One control per host: stdio, the CLI run and cancel all go through it.
    let control = SessionControl::new(harness.clone(), store.clone(), workspace.clone())
        .with_live_config(Some(live_config.clone()))
        .with_questions(Some(questions.clone()))
        .with_agents(Some(agents.clone()))
        .with_wake(Some(agents.wake_coordinator()));
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
    let unattended = one_shot.then(|| {
        deny_unattended(
            harness.clone(),
            store.clone(),
            harness.subscribe(),
            scope.clone(),
        )
    });
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
                agents.resume_quota_wait(wait, approval_policy(&providers.snapshot().settings)?)
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
            Command::Web { port, assets } => {
                let provider_routes = bubblepaw_runner::web_providers::router(providers(
                    &effective,
                    &model_provider,
                    &live_config,
                )?);
                let listener =
                    tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port)).await?;
                let address = listener.local_addr()?;
                diagnostic!("BubblePaw workbench: http://{address}");
                let config = bubblepaw_runner::web::WebConfig {
                    workspace: workspace.to_string_lossy().into_owned(),
                    model: cli.model.clone(),
                    provider: selected_main_provider(&settings).to_owned(),
                    read_only: cli.read_only,
                };
                bubblepaw_runner::web::serve_with_services(
                    listener,
                    harness.clone(),
                    store.clone(),
                    config,
                    assets.clone(),
                    bubblepaw_runner::web::WebServices {
                        session_scope: Some(scope.clone()),
                        storage_policy: Some(settings.root_storage_policy()),
                        provider_routes: Some(provider_routes),
                        providers: Some(model_provider.clone()),
                        live_config: Some(live_config.clone()),
                        context_strategy: settings.context_strategy,
                        agents: Some(agents.clone()),
                        repl: Some(repl.clone()),
                        processes: Some(processes.clone()),
                        questions: Some(questions.clone()),
                        artifacts: Some(artifacts.clone()),
                        input_files: Some(Arc::new(
                            bubblepaw_runner::input_files::InputFiles::new(
                                &workspace,
                                input_limits,
                            )?
                            .with_store(store.clone()),
                        )),
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
            Command::Stdio => {
                run_stdio(
                    harness.clone(),
                    store.clone(),
                    StdioServices {
                        providers: providers(&effective, &model_provider, &live_config)?,
                        usage: usage_settings.clone(),
                        workspace: workspace.clone(),
                        repl: repl.clone(),
                        processes: processes.clone(),
                        context_strategy: settings.context_strategy,
                        agents: agents.clone(),
                        live_config: live_config.clone(),
                        questions: questions.clone(),
                        scope: scope.clone(),
                        storage_policy: settings.root_storage_policy(),
                        control: control.clone(),
                    },
                    input_limits,
                    cancellation.clone(),
                )
                .await
            }
            Command::Run { session, name, .. } => {
                let prompt = task_prompt
                    .as_deref()
                    .expect("run prompt resolved before startup");
                let command = SlashCommand::parse(prompt);
                if let Some(command) = command.filter(|command| !command.needs_session()) {
                    // /reload needs no session, so none is created for it.
                    output.json(&control.execute(None, command).await?)
                } else {
                    let session = open_or_create(&store, session.as_deref(), {
                        let (name, workspace) = (name.clone(), workspace.clone());
                        let strategy = settings.context_strategy;
                        let policy = settings.root_storage_policy();
                        move |store| {
                            let title = session_title(name.as_deref(), "New session")?;
                            store.create_session_with_policy(title, strategy, &workspace, policy)
                        }
                    })
                    .await?;
                    match command {
                        Some(command) => {
                            output.json(&control.execute(Some(&session.id), command).await?)
                        }
                        None => {
                            run_once(
                                harness.clone(),
                                &session.id,
                                Some(prompt),
                                cli.json,
                                output.clone(),
                                cancellation.clone(),
                                scope.clone(),
                            )
                            .await
                        }
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
                let options = RunOptions {
                    script: Some(
                        std::fs::read_to_string(file)
                            .with_context(|| format!("read {}", file.display()))?,
                    ),
                    ..RunOptions::default()
                };
                let session = open_or_create(&store, session.as_deref(), {
                    let (name, workspace) = (name.clone(), workspace.clone());
                    let strategy = settings.context_strategy;
                    let policy = settings.root_storage_policy();
                    move |store| {
                        let title = session_title(name.as_deref(), "Script")?;
                        store.create_session_with_policy(title, strategy, &workspace, policy)
                    }
                })
                .await?;
                let label = format!("[script] {}", file.display());
                run_once_with(
                    harness.clone(),
                    &session.id,
                    Some(&label),
                    options,
                    cli.json,
                    output.clone(),
                    cancellation.clone(),
                    scope.clone(),
                )
                .await
            }
            Command::Resume { session } => {
                run_once(
                    harness.clone(),
                    session,
                    None,
                    cli.json,
                    output.clone(),
                    cancellation.clone(),
                    scope.clone(),
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
        if scope.hold_owned_for_cleanup(&store, &session.id)? && harness.cancel(&session.id) {
            let _ =
                tokio::time::timeout(Duration::from_secs(5), harness.wait_for_idle(&session.id))
                    .await;
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
