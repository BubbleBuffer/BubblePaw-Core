use super::sessions::*;
use super::*;

pub fn open(ctx: &HostCtx, input: OpenParams) -> Result<OpenResult, HostError> {
    if let Some(id) = input.session_id {
        ctx.ensure_session(&id)?;
        let session = ctx.store.session(&id)?;
        if let Some(live) = &ctx.live_config {
            live.initialize_config_cursor(&session.id);
        }
        return Ok(session);
    }
    let title = crate::sessions::title(input.title.as_deref(), "New session")
        .map_err(|e| HostError::invalid(e.to_string()))?;
    let session = if let Some(parent) = input.parent_session_id {
        ctx.ensure_session(&parent)?;
        ctx.store
            .create_session_with_parent(title, ctx.context_strategy, Some(&parent))?
    } else if let Some(policy) = ctx.storage_policy {
        ctx.store
            .create_session_with_policy(title, ctx.context_strategy, &ctx.workspace, policy)?
    } else {
        ctx.store.create_session_with_strategy_and_workspace(
            title,
            ctx.context_strategy,
            &ctx.workspace,
        )?
    };
    if ctx.hold_admission {
        ctx.scope.set_root(&session.id);
    } else {
        ctx.scope.adopt_idle_root(&session.id);
    }
    if let Some(live) = &ctx.live_config {
        live.initialize_config_cursor(&session.id);
    }
    Ok(session)
}
pub fn import(ctx: &HostCtx, input: ImportParams) -> Result<ImportResult, HostError> {
    bubblepaw_core::import_file(
        &ctx.store,
        input.path,
        input.format,
        &bubblepaw_core::ImportOptions {
            title: input.title,
            adjacent_agent_paths: input.children,
            ..Default::default()
        },
    )
    .map_err(Into::into)
}
pub fn search(ctx: &HostCtx, input: SearchParams) -> Result<SearchResult, HostError> {
    if let Some(id) = &input.session_id {
        // An absent/deleted explicit chat is an empty search domain. Preserve
        // the CLI search contract without hiding metadata or storage failures.
        match ctx.store.session_root_id(id) {
            Ok(_) => {}
            Err(error)
                if error
                    .downcast_ref::<rusqlite::Error>()
                    .is_some_and(|error| matches!(error, rusqlite::Error::QueryReturnedNoRows)) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        }
        ctx.ensure_read_session(id)?;
    }
    while !ctx
        .store
        .reindex_search(input.session_id.as_deref(), 512)?
        .complete
    {}
    Ok(ctx
        .store
        .search_messages(
            &input.query,
            input.session_id.as_deref(),
            input.limit.unwrap_or(50).min(1000),
        )?
        .into_iter()
        .filter(|hit| {
            ctx.read_scope
                .allows(&ctx.store, &hit.session_id)
                .unwrap_or(false)
                && (input.session_id.is_some()
                    || ctx.store.session(&hit.session_id).is_ok_and(|s| !s.hidden))
        })
        .collect())
}
pub fn state(ctx: &HostCtx, input: SessionParams) -> Result<StateResult, HostError> {
    let id = &input.session_id;
    ctx.ensure_read_session(id)?;
    let (mut state, position) = ctx.harness.snapshot_with(id, || {
        Ok(json!({
            "session":ctx.store.session(id)?,"messages":ctx.store.messages(id)?,
            "runs":ctx.store.runs(id)?,"inputs":ctx.store.queued_inputs(id)?,
            "outputs":ctx.store.outputs(id,128)?,"pending_effects":ctx.store.pending_effects(id)?,
            "todo":crate::todo::list(&ctx.store,id)?,
            "question":ctx.questions.as_ref().map(|q| q.pending(id)).transpose()?.flatten(),
            "usage":crate::usage::session_usage(&ctx.store,id,&ctx.usage_settings())?
        }))
    })?;
    state["repl"] = ctx.repl.as_ref().map_or(Value::Null, |r| r.status(id));
    state["seq"] = json!(position.seq);
    state["streaming_text"] = json!(position.streaming_text);
    Ok(state)
}
pub fn history(ctx: &HostCtx, input: SessionHistoryParams) -> Result<HistoryPage, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    let limit = input.limit.unwrap_or(200);
    if !(1..=1000).contains(&limit) || input.after_seq < 0 {
        return Err(HostError::invalid("Invalid history page"));
    }
    let mut rows = Vec::new();
    ctx.store.for_each_message_range_while(
        &input.session_id,
        input.after_seq.saturating_add(1),
        i64::MAX,
        |seq, message| {
            rows.push((seq, message));
            Ok(rows.len() <= limit)
        },
    )?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let next_seq = if has_more {
        rows.last().map(|(seq, _)| *seq)
    } else {
        None
    };
    Ok(HistoryPage {
        messages: rows.into_iter().map(|(_, message)| message).collect(),
        next_seq,
    })
}
pub fn messages(ctx: &HostCtx, input: HistoryParams) -> Result<MessagesPage, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    let limit = input.limit.unwrap_or(100);
    if !(1..=200).contains(&limit) {
        return Err(HostError::invalid("Limit must be between 1 and 200"));
    }
    let rows = ctx
        .store
        .messages_before(&input.session_id, input.before.as_deref(), limit + 1)?
        .ok_or_else(|| HostError::invalid("Unknown before message id"))?;
    let has_more = rows.len() > limit;
    let messages = rows
        .into_iter()
        .skip(usize::from(has_more))
        .map(|(_, mut m)| {
            m.provider_state = None;
            m
        })
        .collect();
    Ok(MessagesPage { messages, has_more })
}
pub fn archive(ctx: &HostCtx, input: SessionParams) -> Result<ArchiveResult, HostError> {
    ctx.ensure_session(&input.session_id)?;
    let (changed, worktrees) = crate::worktrees::archive_chat(&ctx.store, &input.session_id)?;
    Ok(ArchiveResult {
        chat: input.session_id,
        changed,
        worktrees,
    })
}
pub fn unarchive(ctx: &HostCtx, input: SessionParams) -> Result<UnarchiveResult, HostError> {
    ctx.ensure_session(&input.session_id)?;
    ctx.store.session(&input.session_id)?;
    Ok(UnarchiveResult {
        chat: input.session_id,
        storage_state: "active".into(),
    })
}
