//! Observing a child's turn through to its reply.
use super::*;

impl AgentTools {
    pub(super) fn observe_turn(
        &self,
        child: Arc<Child>,
        context: &ToolContext,
        dispatcher: Option<ToolDispatcher>,
        mut events: broadcast::Receiver<HarnessEvent>,
    ) {
        let parent_cancel = context.cancellation.clone();
        let invocation = context.invocation_id.clone();
        let repl = child
            .runtime
            .as_ref()
            .map(|runtime| runtime.repl.clone())
            .or_else(|| self.repl.clone());
        let processes = child
            .runtime
            .as_ref()
            .map(|runtime| runtime.processes.clone())
            .or_else(|| self.processes.clone());
        let store = self.store.clone();
        let registry = self.registry.clone();
        let (stop, controls, started, control_error, run_id) = {
            let turn = child.turn.lock().expect("agent turn lock");
            (
                turn.stop.clone(),
                turn.controls.clone(),
                turn.started,
                turn.control_error.clone(),
                turn.run_id.clone().unwrap_or_default(),
            )
        };
        tokio::spawn(async move {
            let mut timed_out = false;
            let mut partial = String::new();
            let mut partial_bytes = 0u64;
            let mut partial_gap = false;
            let deadline = async {
                match controls.timeout {
                    Some(ms) => {
                        tokio::time::sleep(
                            Duration::from_millis(ms).saturating_sub(started.elapsed()),
                        )
                        .await
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(deadline);
            let run = loop {
                break tokio::select! {
                    biased;
                    run=child.harness.wait_for_idle(&child.session_id)=>run,
                    _=stop.cancelled()=> {
                        let descendants=registry.signal(&store,&child.session_id,false);
                        child.harness.cancel(&child.session_id);
                        AgentRegistry::finish_cancel(descendants).await;
                        child.harness.wait_for_idle(&child.session_id).await
                    },
                    _=parent_cancel.cancelled()=> {
                        stop.cancel();
                        let descendants=registry.signal(&store,&child.session_id,false);
                        child.harness.cancel(&child.session_id);
                        AgentRegistry::finish_cancel(descendants).await;
                        child.harness.wait_for_idle(&child.session_id).await
                    },
                    _=&mut deadline=> {
                        timed_out = true;
                        loop {
                            match events.try_recv() {
                                Ok(event) => capture_partial(event, &child.session_id, &run_id, &mut partial, &mut partial_bytes, &mut partial_gap),
                                Err(broadcast::error::TryRecvError::Lagged(_)) => partial_gap = true,
                                Err(_) => break,
                            }
                        }
                        if partial.is_empty() || partial_gap {
                            let snapshot = child.harness.snapshot_with(&child.session_id, || Ok(()))
                                .map(|(_, position)| position.streaming_text).unwrap_or_default();
                            if !snapshot.is_empty() {
                                partial_bytes = snapshot.len() as u64;
                                partial = snapshot;
                                partial_gap = false;
                            }
                        }
                        // The session and handle survive; only this run is cancelled.
                        child.harness.cancel(&child.session_id);
                        child.harness.wait_for_idle(&child.session_id).await
                    },
                    event=events.recv()=> {
                        match event {
                            Ok(event) => capture_partial(event, &child.session_id, &run_id, &mut partial, &mut partial_bytes, &mut partial_gap),
                            Err(broadcast::error::RecvError::Lagged(_)) => partial_gap = true,
                            Err(broadcast::error::RecvError::Closed) => {},
                        }
                        continue;
                    },
                };
            };
            // Even if run completion won the race, cancellation awaits descendants.
            if stop.is_cancelled() || timed_out {
                registry.cancel_children(&store, &child.session_id).await;
            }
            let canonical_terminal = run.as_ref().is_ok_and(|r| {
                store.run(&r.id).is_ok_and(|r| {
                    r.ended_at.is_some()
                        && !matches!(r.status.as_str(), "running" | "waiting_for_quota")
                })
            });
            if let Some(processes) = processes {
                processes.cancel_owner(&child.session_id).await;
            }
            let mut cleanup_error = match repl {
                Some(repl) => close_environments(&repl, &child.session_id).await.err(),
                None => None,
            };
            if child.hidden {
                let released = match tokio::time::timeout(
                    Duration::from_secs(1),
                    child.harness.release_idle_resources(&child.session_id),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!("Worker provider cleanup timed out after 1 second")),
                };
                if let Err(error) = released {
                    cleanup_error = Some(match cleanup_error {
                        Some(previous) => anyhow!("{previous}; {error}"),
                        None => error,
                    });
                }
            }
            let observation_failed = run.is_err();
            let mut reply = match run {
                Ok(mut run) => {
                    let _ = restore_text(&store, &mut run);
                    terminal_reply(&child.handle, &run)
                }
                Err(e) => bounded_reply(
                    json!({"handle":child.handle,"sessionId":child.session_id,"status":"failed","text":"","error":e.to_string()}),
                ),
            };
            let canonical_text = reply["text"].clone();
            if stop.is_cancelled() && reply["status"] == "idle" {
                reply["status"] = json!("cancelled");
                if store
                    .attachment(&child.session_id, CANCEL_ID)
                    .is_ok_and(|m| m.is_none())
                    && let Err(error) =
                        store.store_attachment(&child.session_id, CANCEL_ID, "{}", b"")
                {
                    reply["error"] = json!(format!("Could not persist cancellation: {error}"));
                }
            }
            if timed_out {
                reply["status"] = json!("timed_out");
                if !partial.is_empty() {
                    reply["text"] = json!(partial);
                    if partial_bytes > reply["text"].as_str().unwrap_or("").len() as u64 {
                        reply["textBytes"] = json!(partial_bytes);
                        reply["resultTruncated"] = json!(true);
                    }
                    if partial_gap {
                        reply["textIncomplete"] = json!(true);
                    }
                }
                reply["error"] = json!(format!(
                    "Child run exceeded timeout of {} milliseconds",
                    controls.timeout.unwrap_or_default()
                ));
            } else if reply["status"] == "idle" {
                add_json(&mut reply, &controls);
            }
            if let Some(error) = control_error {
                reply["status"] = json!("failed");
                reply["error"] = json!(format!("Could not persist child run controls: {error}"));
            }
            if let Some(error) = &cleanup_error {
                reply["runStatus"] = reply["status"].clone();
                reply["status"] = json!("failed");
                let prior = reply["error"].as_str().unwrap_or("");
                reply["error"] = json!(format!(
                    "{prior}{}Child resource cleanup failed: {error}",
                    if prior.is_empty() { "" } else { "; " }
                ));
                reply = bounded_reply(reply);
            }
            reply = bounded_reply(reply);
            // An unfinished provider stream is a live result only. Durable
            // timeout metadata may retain completed canonical assistant text.
            let mut persisted_reply = reply.clone();
            if timed_out {
                persisted_reply["text"] = canonical_text;
                let object = persisted_reply.as_object_mut().expect("child reply object");
                for key in ["textBytes", "textIncomplete", "resultTruncated"] {
                    object.remove(key);
                }
                persisted_reply = bounded_reply(persisted_reply);
            }
            if (cleanup_error.is_some()
                || observation_failed
                || timed_out
                || reply["status"] == "failed")
                && let Ok(turn) = child.turn.lock()
                && let Some(run_id) = &turn.run_id
                && let Err(error) = store.store_attachment(
                    &child.session_id,
                    &format!("{FAILURE_PREFIX}{run_id}"),
                    &persisted_reply.to_string(),
                    b"",
                )
            {
                let prior = reply["error"].as_str().unwrap_or("");
                reply["error"] =
                    json!(format!("{prior}; Could not persist child failure: {error}"));
                reply = bounded_reply(reply);
            }
            if child.binding.worker {
                let finished = WorkerFinished {
                    run_id: run_id.clone(),
                    status: reply["status"].as_str().unwrap_or("failed").into(),
                    finished_at: chrono::Utc::now().to_rfc3339(),
                };
                if let Err(error) = store.store_attachment(
                    &child.session_id,
                    &format!("{WORKER_FINISHED_ID}:{run_id}"),
                    &serde_json::to_string(&finished).expect("worker completion metadata"),
                    b"",
                ) {
                    reply["status"] = json!("failed");
                    reply["error"] = json!(format!("Could not persist worker completion: {error}"));
                }
            }
            if canonical_terminal
                && cleanup_error.is_none()
                && let Some(dispatcher) = dispatcher
                && let Err(error) = dispatcher.complete_effect(&invocation)
            {
                tracing::warn!(
                    target: "bubblepaw::effects",
                    invocation = %invocation,
                    %error,
                    "could not complete a finished worker effect"
                );
            }
            let notice = if timed_out {
                // Completion notices can become durable generated parent input.
                // Keep unfinished streaming text exclusively in the live result.
                let mut canonical_reply = reply.clone();
                canonical_reply["text"] = persisted_reply["text"].clone();
                child_notice(&child, &run_id, &canonical_reply)
            } else {
                child_notice(&child, &run_id, &reply)
            };
            if let Ok(mut turn) = child.turn.lock() {
                turn.result = Some(reply);
                turn.unread = true;
            }
            if !child.binding.worker {
                registry.wake.publish_reminder_if(
                    &child.owner,
                    child_notice_key(&child.session_id, &run_id),
                    reminder_notices::child_completion(&child.session_id, &run_id, notice),
                    || {
                        child.turn.lock().is_ok_and(|turn| {
                            turn.unread && turn.run_id.as_deref() == Some(&run_id)
                        })
                    },
                );
            }
            child.changed.notify_waiters();
            let limits = registry.limits.get();
            let _ = cleanup_workers_at(&store, &registry, &limits, chrono::Utc::now());
            report_worktree_cleanup(&store, &registry.output).await;
        });
    }
}
