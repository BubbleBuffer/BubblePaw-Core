//! Child lookup, status, wait and cancellation.
use super::*;

impl AgentTools {
    pub(super) fn child(&self, owner: &str, address: &str) -> Result<Arc<Child>> {
        self.find_child(owner, address, true)
    }
    pub(super) fn find_child(
        &self,
        owner: &str,
        address: &str,
        require_retention: bool,
    ) -> Result<Arc<Child>> {
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow!("agent registry poisoned"))?;
        if let Some(child) = state
            .children
            .get(address)
            .or_else(|| state.children.values().find(|c| c.session_id == address))
        {
            ensure!(
                child.owner == owner,
                "agent belongs to another conversation"
            );
            return Ok(child.clone());
        }
        ensure!(
            !address.starts_with("agent:"),
            "unknown or expired agent; use its sessionId to reattach"
        );
        let session = self.store.session(address)?;
        ensure!(
            session.parent_session_id.as_deref() == Some(owner),
            "agent belongs to another conversation"
        );
        let binding = match self.store.attachment(address, BINDING_ID)? {
            Some((metadata, _)) => serde_json::from_str::<Binding>(&metadata)?,
            // Old children have no binding; only the restricted default is safe.
            None => Binding {
                profile: "read-only".into(),
                model: None,
                max_rounds: self
                    .stored_profile("read-only")
                    .ok_or_else(|| anyhow!("legacy child requires read-only profile"))?
                    .max_rounds,
                approval: Some(ApprovalPolicy {
                    confirm: ApprovalMode::Deny,
                    tools: Default::default(),
                    deny: Default::default(),
                    timeout_secs: None,
                    hardline: true,
                }),
                worker: false,
            },
        };
        let original_profile = self.stored_profile(&binding.profile).is_some();
        let profile = self
            .stored_profile(&binding.profile)
            // A terminal worker may be inspected after restart without rebuilding
            // its dynamic profile. Workers never resume through the user send API.
            .or_else(|| binding.worker.then(|| self.profiles().first()).flatten())
            .ok_or_else(|| anyhow!("original agent profile {} is unavailable", binding.profile))?;
        let (runtime, workspace_unavailable) = if binding.worker {
            (None, false)
        } else {
            match self.workspace_runtime(address, &binding.profile) {
                Ok(runtime) => (runtime, false),
                Err(_) if !require_retention => (None, true),
                Err(error) => return Err(error),
            }
        };
        let profile = runtime
            .as_ref()
            .map(|runtime| &runtime.profile)
            .unwrap_or(profile);
        let mut latest = self.store.runs(address)?.into_iter().last();
        if let Some(run) = &mut latest {
            restore_text(&self.store, run)?;
        }
        ensure!(
            latest
                .as_ref()
                .is_none_or(|r| !matches!(r.status.as_str(), "running" | "waiting_for_quota")),
            "child has an unresolved run; recover the store before reattaching"
        );
        let retained = if workspace_unavailable {
            false
        } else {
            match self.retain_slot(&mut state) {
                Ok(()) => true,
                Err(error) if require_retention => return Err(error),
                Err(_) => false,
            }
        };
        let handle = format!("agent:{}", uuid::Uuid::new_v4());
        let mut result = latest
            .as_ref()
            .map(|r| terminal_reply(&handle, r))
            .unwrap_or_else(
                || json!({"handle":handle,"sessionId":address,"status":"idle","text":""}),
            );
        if latest
            .as_ref()
            .is_some_and(|r| matches!(r.status.as_str(), "interrupted" | "unknown"))
        {
            result["runStatus"] = result["status"].clone();
            result["status"] = json!("idle");
        }
        if let Some(run) = &latest
            && let Some((metadata, _)) = self
                .store
                .attachment(address, &format!("{FAILURE_PREFIX}{}", run.id))?
        {
            result = serde_json::from_str(&metadata)?;
            result["handle"] = json!(handle);
            result["sessionId"] = json!(address);
        }
        if result["status"] == "idle"
            && cancelled_run(&self.store, address, latest.as_ref().map(|r| r.id.as_str()))?
        {
            result["status"] = json!("cancelled");
        }
        let controls = latest
            .as_ref()
            .map(|run| load_controls(&self.store, address, &run.id))
            .transpose()?
            .unwrap_or_default();
        if result["status"] == "idle" {
            add_json(&mut result, &controls);
        }
        let run_id = latest.as_ref().map(|r| r.id.clone());
        let unread = match &run_id {
            Some(run_id) => self
                .store
                .attachment(address, &format!("{READ_PREFIX}{run_id}"))?
                .is_none(),
            None => false,
        };
        if !retained {
            result["handle"] = Value::Null;
        }
        let mut binding = binding;
        // Ordinary child round limits snapshot host policy, rather than a
        // caller-supplied child control. Refresh on reattachment so a removed
        // or raised profile limit does not survive in old bindings or wakes.
        // Internal worker profiles retain their task-specific frozen budget.
        if !binding.worker {
            binding.max_rounds = profile.max_rounds;
        }
        if binding.model.is_none() {
            binding.model = latest
                .map(|r| r.model)
                .filter(|m| m != bubblepaw_core::SCRIPT_MODEL);
        }
        let child = Arc::new(Child {
            owner: owner.into(),
            session_id: address.into(),
            handle: handle.clone(),
            created: Instant::now(),
            store: self.store.clone(),
            harness: profile.harness.clone(),
            runtime: runtime.clone(),
            binding: binding.clone(),
            original_profile,
            hidden: session.hidden,
            depth: self.depth(address)?,
            turn: Mutex::new(Turn {
                result: Some(result),
                unread,
                run_id,
                stop: CancellationToken::new(),
                cancelling: false,
                controls,
                approval: binding.approval.clone().unwrap_or_default(),
                control_error: None,
                started: Instant::now(),
                last_interaction: Instant::now(),
                last_reminder: None,
                active_waits: 0,
            }),
            changed: Notify::new(),
            wake: Arc::downgrade(&self.registry.wake),
        });
        if retained {
            state.children.insert(handle, child.clone());
        }
        Ok(child)
    }
    pub fn status(&self, owner: &str, input: &Value) -> Result<Value> {
        let object = input
            .as_object()
            .ok_or_else(|| anyhow!("agent_status expects an object"))?;
        ensure!(
            object
                .keys()
                .all(|k| matches!(k.as_str(), "handle" | "sessionId")),
            "agent_status accepts only handle or sessionId"
        );
        if object.is_empty() {
            let sessions = self.store.list_sessions()?;
            let ids = subtree_ids(&sessions, owner);
            let hidden_owner = self.store.session(owner)?.hidden;
            let children=sessions.into_iter().filter(|s| s.id != owner && ids.contains(&s.id) && (!s.hidden || hidden_owner)).map(|s| {
                let internal = self.store.attachment(&s.id, BINDING_ID)?.map(|(metadata, _)| serde_json::from_str::<Binding>(&metadata)).transpose()?.is_some_and(|binding| binding.worker);
                if internal { return Ok(None); }
                let retained={let state=self.registry.state.lock().map_err(|_| anyhow!("agent registry poisoned"))?;state.children.values().find(|c| c.session_id==s.id).cloned()};
                let reply=match retained {Some(child)=>{touch(&child)?;snapshot(&child,false)?},None=>self.archived_status(&s.id)?};
                Ok(Some(json!({"handle":reply["handle"],"sessionId":reply["sessionId"],"parentSessionId":s.parent_session_id,"status":reply["status"],"state":reply["state"],"unread":reply["unread"],"workspace":reply["workspace"],"branch":reply["branch"],"worktree":reply["worktree"]})))
            }).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect::<Vec<_>>();
            return Ok(json!({"children":children}));
        }
        let child = self.find_child(owner, address(input, &["handle", "sessionId"])?, false)?;
        snapshot(&child, true)
    }
    pub(super) fn archived_status(&self, session: &str) -> Result<Value> {
        let latest = self.store.runs(session)?.into_iter().last();
        let mut state = latest
            .as_ref()
            .map(|run| match run.status.as_str() {
                "completed" | "interrupted" => "idle",
                "running" => "running",
                "waiting_for_quota" => "waiting_for_quota",
                "cancelled" => "cancelled",
                _ => "failed",
            })
            .unwrap_or("idle")
            .to_owned();
        let mut unread = false;
        if let Some(run) = latest {
            unread = self
                .store
                .attachment(session, &format!("{READ_PREFIX}{}", run.id))?
                .is_none();
            if let Some((metadata, _)) = self
                .store
                .attachment(session, &format!("{FAILURE_PREFIX}{}", run.id))?
            {
                let reply: Value = serde_json::from_str(&metadata)?;
                state = reply["status"].as_str().unwrap_or("failed").into();
            }
        }
        let latest_id = self.store.runs(session)?.into_iter().last().map(|r| r.id);
        if state == "idle" && cancelled_run(&self.store, session, latest_id.as_deref())? {
            state = "cancelled".into();
        }
        let mut reply = json!({"handle":null,"sessionId":session,"parentSessionId":self.store.session(session)?.parent_session_id,"status":state,"state":state,"unread":unread});
        workspace_reply(&self.store, session, &mut reply)?;
        if let Some(wait) = self.store.quota_wait(session)? {
            reply["status"] = json!("waiting_for_quota");
            reply["state"] = json!("waiting_for_quota");
            reply["quotaWait"] = serde_json::to_value(wait)?;
        }
        if let Some(question) = pending_question(&self.store, session)? {
            reply["status"] = json!("waiting_for_answer");
            reply["state"] = json!("waiting_for_answer");
            reply["question"] = question;
        }
        Ok(reply)
    }
    pub async fn wait(&self, owner: &str, address: &str, timeout_ms: Option<u64>) -> Result<Value> {
        let child = self.find_child(owner, address, false)?;
        let _interaction = WaitInteraction::new(child.clone())?;
        if let Some(ms) = timeout_ms {
            if tokio::time::timeout(
                Duration::from_millis(ms),
                await_child(&child, self.registry.questions.get().map(Arc::as_ref)),
            )
            .await
            .is_err()
            {
                return Ok(
                    json!({"handle":child.handle,"sessionId":child.session_id,"status":"timeout"}),
                );
            }
        } else {
            await_child(&child, self.registry.questions.get().map(Arc::as_ref)).await?;
        }
        let mut reply = snapshot(&child, true)?;
        if reply["status"] == "idle" {
            reply["state"] = json!("idle");
            reply["status"] = json!("completed");
        }
        reply
            .as_object_mut()
            .expect("agent reply object")
            .remove("unread");
        Ok(reply)
    }
    pub async fn cancel(&self, owner: &str, address: &str) -> Result<Value> {
        let child = self.find_child(owner, address, false)?;
        let cancelled_run_id = child
            .turn
            .lock()
            .map_err(|_| anyhow!("agent result poisoned"))?
            .run_id
            .clone();
        self.registry.wake.suppress(&child.session_id);
        self.mark_catalog_cancel(&child.session_id, true)?;
        self.registry
            .cancel_tree(&self.store, &child.session_id, true)
            .await;
        // A temporary result reader may not be retained when slots are busy.
        // Apply the intent to that snapshot, without touching a newer run.
        if let Ok(mut turn) = child.turn.lock()
            && turn.run_id == cancelled_run_id
            && let Some(reply) = turn.result.as_mut()
            && reply["status"] == "idle"
        {
            reply["status"] = json!("cancelled");
        }
        let mut reply = snapshot(&child, false)?;
        reply
            .as_object_mut()
            .expect("agent reply object")
            .remove("unread");
        Ok(reply)
    }
    /// Synchronous harness hook signals the subtree deepest first. The running
    /// turn observers wait for descendants before publishing terminal results.
    pub fn cancel_session(&self, owner: &str) -> usize {
        // A tree signal calls Harness::cancel on each child. Its nested tool
        // hook must not traverse that already-signalled tree again before the
        // harness gets to cancel its own run. The durable intent was written
        // for the complete subtree before its first signal.
        let signalled = self.registry.state.lock().is_ok_and(|state| {
            state.children.values().any(|c| {
                c.session_id == owner && c.turn.lock().is_ok_and(|t| t.stop.is_cancelled())
            })
        });
        if signalled
            && self
                .store
                .attachment(owner, CANCEL_ID)
                .is_ok_and(|m| m.is_some())
        {
            return 0;
        }
        let _ = self.mark_catalog_cancel(owner, false);
        let children = self.registry.signal(&self.store, owner, false);
        let count = children.len();
        if !children.is_empty()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(AgentRegistry::finish_cancel(children));
        }
        count
    }
    /// Persist cancellation intent for every canonical descendant, even when
    /// its inactive handle expired. Resources are still awaited separately.
    fn mark_catalog_cancel(&self, owner: &str, include_owner: bool) -> Result<()> {
        let ids = subtree_ids(&self.store.list_sessions()?, owner);
        for id in ids.into_iter().filter(|id| include_owner || id != owner) {
            if let Some(questions) = self.registry.questions.get() {
                questions.cancel(&id)?;
            }
            if self.store.attachment(&id, CANCEL_ID)?.is_none() {
                self.store.store_attachment(&id, CANCEL_ID, "{}", b"")?;
            }
            if let Some(run) = self.store.runs(&id)?.into_iter().last() {
                let marker = format!("{CANCEL_PREFIX}{}", run.id);
                if self.store.attachment(&id, &marker)?.is_none() {
                    self.store.store_attachment(&id, &marker, "{}", b"")?;
                }
            }
        }
        Ok(())
    }
    pub async fn cancel_children(&self, owner: &str) -> Result<()> {
        self.mark_catalog_cancel(owner, false)?;
        self.registry.cancel_children(&self.store, owner).await;
        Ok(())
    }
    pub async fn shutdown(&self) -> Result<()> {
        let children = {
            let mut state = self
                .registry
                .state
                .lock()
                .map_err(|_| anyhow!("agent registry poisoned"))?;
            state.stopping = true;
            let mut children = state
                .children
                .values()
                .filter(|c| c.turn.lock().is_ok_and(|t| t.result.is_none()))
                .cloned()
                .collect::<Vec<_>>();
            children.sort_by_key(|c| std::cmp::Reverse(c.depth));
            children
                .into_iter()
                .map(|child| {
                    let run_id = child.turn.lock().ok().and_then(|mut turn| {
                        turn.cancelling = true;
                        turn.stop.cancel();
                        turn.run_id.clone()
                    });
                    CancelledTurn { child, run_id }
                })
                .collect::<Vec<_>>()
        };
        AgentRegistry::finish_cancel(children).await;
        Ok(())
    }
}
