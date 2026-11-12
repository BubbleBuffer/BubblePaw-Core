//! Approval clamping, profile validation, snapshots and worker cleanup.
use super::*;

pub(super) fn run_options(
    binding: &Binding,
    approval: ApprovalPolicy,
    controls: &Controls,
) -> RunOptions {
    RunOptions {
        origin: MessageOrigin::ChildTask,
        model: binding.model.clone(),
        max_rounds: binding.max_rounds,
        approval: Some(approval),
        output_schema: controls.output_schema.clone(),
        max_tool_calls: controls.max_tool_calls,
        ..Default::default()
    }
}
/// Deny < Ask < Auto < Allow, applied to default Confirm and every named rule.
pub fn clamp_approval(parent: &ApprovalPolicy, profile: Option<&ApprovalPolicy>) -> ApprovalPolicy {
    let Some(profile) = profile else {
        return parent.clone();
    };
    let mut policy = ApprovalPolicy {
        confirm: stricter(parent.confirm, profile.confirm),
        tools: Default::default(),
        deny: profile.clone().with_inherited_denies(parent).deny,
        timeout_secs: parent.timeout_secs,
        // Children inherit the host's hardline floor; a profile cannot turn it off.
        hardline: parent.hardline,
    };
    for name in parent.tools.keys().chain(profile.tools.keys()) {
        // A named override may address a confirming capability. Clamp against
        // that default too, conservatively tightening non-confirming tools.
        policy.tools.insert(
            name.clone(),
            stricter(
                parent.mode(name, Approval::Confirm),
                profile.mode(name, Approval::Confirm),
            ),
        );
    }
    policy
}
fn stricter(a: ApprovalMode, b: ApprovalMode) -> ApprovalMode {
    fn rank(m: ApprovalMode) -> u8 {
        match m {
            ApprovalMode::Deny => 0,
            ApprovalMode::Ask => 1,
            ApprovalMode::Auto => 2,
            ApprovalMode::Allow => 3,
        }
    }
    if rank(a) <= rank(b) { a } else { b }
}
impl AgentTools {
    pub(super) fn profile_summaries(&self) -> Vec<(String, String)> {
        self.profiles()
            .iter()
            .filter(|profile| self.profile_enabled(&profile.name))
            .map(|p| (p.name.clone(), p.description.clone()))
            .collect()
    }
}
pub(super) fn cap(limit: Option<usize>) -> String {
    limit.map_or_else(|| "unlimited".into(), |value| value.to_string())
}
pub(super) fn minimum<T: Ord>(requested: Option<T>, inherited: Option<T>) -> Option<T> {
    match (requested, inherited) {
        (Some(requested), Some(inherited)) => Some(requested.min(inherited)),
        (requested, inherited) => requested.or(inherited),
    }
}
pub(super) fn worker_finished(
    store: &Store,
    session: &str,
    run: Option<&str>,
) -> Result<Option<WorkerFinished>> {
    let current = run
        .map(|run| store.attachment(session, &format!("{WORKER_FINISHED_ID}:{run}")))
        .transpose()?
        .flatten();
    current
        .or(store.attachment(session, WORKER_FINISHED_ID)?)
        .map(|(metadata, _)| serde_json::from_str(&metadata).map_err(Into::into))
        .transpose()
}
pub(super) fn validate_profiles(profiles: &[AgentProfile]) -> Result<()> {
    ensure!(
        !profiles.is_empty(),
        "at least one agent profile is required"
    );
    let mut names = HashSet::new();
    for profile in profiles {
        ensure!(
            !profile.name.trim().is_empty(),
            "agent profile name must not be empty"
        );
        ensure!(
            names.insert(profile.name.clone()),
            "duplicate agent profile: {}",
            profile.name
        );
        ensure!(
            profile.max_rounds != Some(0),
            "agent profile max_rounds must be positive"
        );
    }
    Ok(())
}
/// Session ids in `owner`'s catalog subtree, including `owner`.
pub(super) fn subtree_ids(sessions: &[Session], owner: &str) -> HashSet<String> {
    let mut ids = HashSet::from([owner.to_owned()]);
    loop {
        let before = ids.len();
        for session in sessions {
            if session
                .parent_session_id
                .as_ref()
                .is_some_and(|p| ids.contains(p))
            {
                ids.insert(session.id.clone());
            }
        }
        if before == ids.len() {
            return ids;
        }
    }
}
pub(super) fn address<'a>(input: &'a Value, allowed: &[&str]) -> Result<&'a str> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("agent operation expects an object"))?;
    ensure!(
        object.keys().all(|k| allowed.contains(&k.as_str())),
        "unexpected agent argument"
    );
    ensure!(
        input.get("handle").is_none() || input.get("sessionId").is_none(),
        "provide either handle or sessionId"
    );
    let value = input
        .get("handle")
        .or_else(|| input.get("sessionId"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("handle or sessionId must be a string"))?;
    ensure!(!value.is_empty(), "handle or sessionId must not be empty");
    Ok(value)
}
pub(super) fn snapshot(child: &Child, consume: bool) -> Result<Value> {
    if consume && let Some(wake) = child.wake.upgrade() {
        return wake.consume(&child.owner, || snapshot_inner(child, true));
    }
    snapshot_inner(child, consume).map(|(reply, _)| reply)
}
fn snapshot_inner(child: &Child, consume: bool) -> Result<(Value, Option<String>)> {
    let mut turn = child
        .turn
        .lock()
        .map_err(|_| anyhow!("agent result poisoned"))?;
    if consume {
        turn.last_interaction = Instant::now();
        turn.last_reminder = None;
    }
    let mut reply = turn.result.clone().unwrap_or_else(
        || json!({"handle":child.handle,"sessionId":child.session_id,"status":"running"}),
    );
    let quota = child.store.quota_wait(&child.session_id)?;
    if let Some(wait) = &quota {
        reply["status"] = json!("waiting_for_quota");
        reply["quotaWait"] = serde_json::to_value(wait)?;
    }
    if let Some(question) = pending_question(&child.store, &child.session_id)? {
        reply["status"] = json!("waiting_for_answer");
        reply["question"] = question;
    }
    reply["state"] = reply["status"].clone();
    reply["parentSessionId"] = json!(child.owner);
    reply["unread"] = json!(turn.unread);
    if consume && turn.result.is_some() && quota.is_none() {
        if turn.unread
            && let Some(run_id) = &turn.run_id
        {
            let receipt = format!("{READ_PREFIX}{run_id}");
            if child
                .store
                .attachment(&child.session_id, &receipt)?
                .is_none()
            {
                child
                    .store
                    .store_attachment(&child.session_id, &receipt, "{}", b"")?;
            }
        }
        turn.unread = false;
    }
    let source = if consume && turn.result.is_some() && quota.is_none() {
        turn.run_id
            .as_ref()
            .map(|run| child_notice_key(&child.session_id, run))
    } else {
        None
    };
    workspace_reply(&child.store, &child.session_id, &mut reply)?;
    Ok((bounded_reply(reply), source))
}
pub(super) fn workspace_reply(store: &Store, session: &str, reply: &mut Value) -> Result<()> {
    let mut current = Some(session.to_owned());
    let mut seen = HashSet::new();
    while let Some(id) = current {
        ensure!(seen.insert(id.clone()), "session ancestry contains a cycle");
        if let Some(info) = crate::worktrees::get(store, &id)? {
            reply["workspace"] = json!(info.path);
            reply["branch"] = json!(info.branch);
            reply["worktreeAvailable"] = json!(info.path.is_dir());
            reply["worktree"] = serde_json::to_value(info)?;
            reply["note"] = if id == session {
                json!("Parent uncommitted changes are not copied into the child worktree.")
            } else {
                json!("Uses the parent's existing worktree; uncommitted changes are shared.")
            };
            break;
        }
        current = store.session(&id)?.parent_session_id;
    }
    Ok(())
}
pub(super) fn cleanup_workers_at(
    store: &Store,
    registry: &AgentRegistry,
    limits: &Agents,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<usize> {
    if limits.worker_keep_all {
        return Ok(0);
    }
    let cutoff = now
        .checked_sub_signed(chrono::Duration::days(i64::from(
            limits.worker_retention_days,
        )))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
    let sessions = store.list_sessions()?;
    let mut state = registry
        .state
        .lock()
        .map_err(|_| anyhow!("agent registry poisoned"))?;
    let mut deleted = HashSet::new();
    for session in &sessions {
        if registry
            .scope
            .as_ref()
            .is_some_and(|scope| !scope.contains(store, &session.id))
            || !session.hidden
            || deleted.contains(&session.id)
            || store.active_session_metadata(&session.id)?.is_none()
        {
            continue;
        }
        let Some((metadata, _)) = store.attachment(&session.id, BINDING_ID)? else {
            continue;
        };
        if !serde_json::from_str::<Binding>(&metadata)?.worker {
            continue;
        }
        let mut tree = subtree_ids(&sessions, &session.id);
        // Earlier candidates can delete a descendant before its ancestor is
        // inspected. Preserve ancestry from the snapshot, but exclude removed
        // records from checks and deletion counts.
        tree.retain(|id| !deleted.contains(id));
        let mut eligible = true;
        for id in &tree {
            let retained = state
                .children
                .values()
                .find(|child| child.session_id == *id);
            if let Some(child) = retained {
                let turn = child
                    .turn
                    .lock()
                    .map_err(|_| anyhow!("agent result poisoned"))?;
                if turn.result.is_none() || (limits.worker_retention_days == 0 && turn.unread) {
                    eligible = false;
                    break;
                }
            }
            if store.quota_wait(id)?.is_some() || !store.pending_effects(id)?.is_empty() {
                eligible = false;
                break;
            }
            let latest = store.runs(id)?.into_iter().last();
            if latest
                .as_ref()
                .is_some_and(|run| matches!(run.status.as_str(), "running" | "waiting_for_quota"))
            {
                eligible = false;
                break;
            }
            let finished = worker_finished(store, id, latest.as_ref().map(|run| run.id.as_str()))?;
            if finished
                .as_ref()
                .zip(latest.as_ref())
                .is_some_and(|(finished, run)| finished.run_id != run.id)
            {
                eligible = false;
                break;
            }
            let cancelled = cancelled_run(
                store,
                id,
                finished
                    .as_ref()
                    .map(|value| value.run_id.as_str())
                    .or_else(|| latest.as_ref().map(|run| run.id.as_str())),
            )?;
            let internal_worker = if *id == session.id {
                true
            } else {
                store
                    .attachment(id, BINDING_ID)?
                    .map(|(metadata, _)| serde_json::from_str::<Binding>(&metadata))
                    .transpose()?
                    .is_some_and(|binding| binding.worker)
            };
            // Only explicit acknowledgement may delete successful workers.
            // A crashed observer without a completion marker is conservative too.
            if internal_worker
                && !cancelled
                && (finished
                    .as_ref()
                    .is_some_and(|value| value.status == "idle")
                    || (finished.is_none()
                        && latest.as_ref().is_some_and(|run| run.status == "completed")))
            {
                eligible = false;
                break;
            }
            let ended = finished
                .as_ref()
                .map(|value| value.finished_at.as_str())
                .or_else(|| latest.as_ref().and_then(|run| run.ended_at.as_deref()))
                .unwrap_or_else(|| {
                    sessions
                        .iter()
                        .find(|session| session.id == *id)
                        .map_or(session.created_at.as_str(), |session| {
                            session.created_at.as_str()
                        })
                });
            if chrono::DateTime::parse_from_rfc3339(ended)?.with_timezone(&chrono::Utc) > cutoff {
                eligible = false;
                break;
            }
        }
        if eligible {
            // Admission and unresolved-effect checks repeat atomically at deletion.
            store.delete_session_preserving_effects(&session.id)?;
            state
                .children
                .retain(|_, child| !tree.contains(&child.session_id));
            deleted.extend(tree);
        }
    }
    if !deleted.is_empty() {
        registry
            .approval_sources
            .lock()
            .expect("child approval sources lock")
            .retain(|session, _| !deleted.contains(session));
    }
    Ok(deleted.len())
}
