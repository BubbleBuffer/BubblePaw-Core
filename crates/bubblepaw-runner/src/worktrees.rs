//! Opt-in git worktrees for child sessions. A child gets `bp/<child-id>` checked
//! out under the store's data directory, created from the parent workspace's
//! current HEAD. Git runs with argument vectors only; no shell is involved.
//! Uncommitted parent changes are not copied.
//!
//! The session attachment is the authority for active lookups. A small mirror
//! under `worktrees/.bindings/<session>.json` lets cleanup run for archived or
//! deleted sessions without opening their SQLite files, which would restore them.
use anyhow::{Context, Result, anyhow, bail, ensure};
use bubblepaw_core::Store;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Mutex, MutexGuard},
};

const ATTACHMENT: &str = "bubblepaw-worktree-v1";
const BRANCH_PREFIX: &str = "bp/";
const BINDINGS_DIR: &str = ".bindings";

// Filesystem mutations and run admission share this short boundary; child runs
// remain concurrent. Cleanup must not remove a checkout between restore/start.
static ADMISSION: Mutex<()> = Mutex::new(());
pub(crate) fn admission_guard() -> Result<MutexGuard<'static, ()>> {
    ADMISSION
        .lock()
        .map_err(|_| anyhow!("worktree admission lock poisoned"))
}

/// The immutable binding of one child session to its worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    pub parent_workspace: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupStatus {
    Removed,
    Retained,
    Missing,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupReport {
    pub session_id: String,
    pub path: PathBuf,
    pub branch: String,
    pub status: CleanupStatus,
    pub note: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MirrorState {
    /// The worktree exists and belongs to this binding.
    Active,
    /// Cleanup removed the worktree; only `restore` may recreate it.
    Removed,
}

/// Out-of-tree copy of a binding. Only `state` changes after it is written.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Mirror {
    session_id: String,
    path: PathBuf,
    branch: String,
    parent_workspace: PathBuf,
    state: MirrorState,
}

/// Creates the child's worktree and binds it to the child session.
pub fn create(
    store: &Store,
    parent_workspace: &Path,
    child_session_id: &str,
) -> Result<WorktreeInfo> {
    let _guard = admission_guard()?;
    create_in_admission(store, parent_workspace, child_session_id)
}
pub(crate) fn create_in_admission(
    store: &Store,
    parent_workspace: &Path,
    child_session_id: &str,
) -> Result<WorktreeInfo> {
    validate_id(child_session_id)?;
    let data_dir = data_dir_of(store)?;
    if let Some(existing) = get(store, child_session_id)? {
        bail!(
            "session {child_session_id} already has a worktree at {}; use get to inspect it",
            existing.path.display()
        );
    }
    ensure!(
        !occupied(&mirror_file(&data_dir, child_session_id)),
        "a binding mirror already exists for session {child_session_id}; inspect it before creating"
    );
    let parent = canonical(parent_workspace).with_context(|| {
        format!(
            "parent workspace {} is not accessible",
            parent_workspace.display()
        )
    })?;
    let repository = git(&parent, &["rev-parse", "--show-toplevel"]).with_context(|| {
        format!(
            "parent workspace {} is not a git repository; child worktrees need git",
            parent.display()
        )
    })?;
    ensure!(
        !data_dir.starts_with(canonical(Path::new(repository.trim()))?),
        "child worktrees require a BubblePaw data directory outside the parent repository"
    );
    ensure!(
        git_test(
            &parent,
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]
        )?,
        "parent repository {} has no commits; commit before starting a child worktree",
        parent.display()
    );

    let branch = branch_name(child_session_id);
    let root = worktrees_root(&data_dir);
    let path = root.join(child_session_id);
    ensure!(
        !occupied(&path),
        "worktree path {} already exists; move it aside first",
        path.display()
    );
    let full_branch = format!("refs/heads/{branch}");
    ensure!(
        !git_test(
            &parent,
            &["rev-parse", "--verify", "--quiet", full_branch.as_str()]
        )?,
        "branch {branch} already exists in {}",
        parent.display()
    );
    let info = WorktreeInfo {
        path,
        branch,
        parent_workspace: parent.clone(),
    };
    let metadata = serde_json::to_string(&info)?;
    fs::create_dir_all(&root).with_context(|| format!("could not create {}", root.display()))?;

    git(
        &parent,
        &[
            "worktree",
            "add",
            "-b",
            info.branch.as_str(),
            utf8(&info.path)?,
            "HEAD",
        ],
    )
    .context("git could not create the child worktree")?;

    if let Err(error) = write_mirror(&data_dir, child_session_id, &info, MirrorState::Active) {
        return Err(abandon(&parent, &data_dir, child_session_id, &info, error));
    }
    if let Err(save_error) = store.store_attachment(child_session_id, ATTACHMENT, &metadata, b"") {
        settle_failed_save(
            store,
            &data_dir,
            child_session_id,
            &parent,
            &info,
            save_error,
        )?;
    }
    Ok(info)
}

/// Returns this session's own binding, without walking ancestors.
pub fn get(store: &Store, session: &str) -> Result<Option<WorktreeInfo>> {
    match store.attachment(session, ATTACHMENT)? {
        None => Ok(None),
        Some((metadata, _)) => Ok(Some(serde_json::from_str(&metadata).with_context(
            || format!("worktree binding of session {session} is unreadable"),
        )?)),
    }
}

/// The nearest worktree along the catalog parent chain, or `fallback` when no
/// ancestor (including the session itself) has one. A bound path that has gone
/// missing is an error, never a silent fallback to the parent workspace.
/// Reads attachments, so call it only for active sessions.
/// Worktree inheritance follows logical ancestry, including old cross-file
/// forks. The core separately protects the physical root chat's workspace;
/// an inherited explicit worktree is its additional execution workspace.
pub fn workspace(store: &Store, session: &str, fallback: &Path) -> Result<PathBuf> {
    let mut seen = HashSet::new();
    let mut current = session.to_owned();
    loop {
        ensure!(
            seen.insert(current.clone()),
            "session ancestry of {session} contains a cycle"
        );
        if let Some(info) = get(store, &current)? {
            ensure!(
                info.path.join(".git").exists(),
                "worktree of session {current} is missing or moved to {}; restore it before running tools",
                info.path.display()
            );
            return Ok(info.path);
        }
        match store.session(&current)?.parent_session_id {
            Some(parent) => current = parent,
            None => {
                return Ok(store
                    .session_workspace(session)?
                    .map_or_else(|| fallback.to_path_buf(), |path| plain(&path)));
            }
        }
    }
}

/// Recreates a worktree that `cleanup` removed, for an explicit send. The session
/// is opened first, which restores an archived chat as any send does. The saved
/// branch is reused when it still exists; otherwise `bp/<session>` is created from
/// the nearest ancestor worktree's HEAD, or from the original parent workspace.
/// Missing paths that cleanup did not remove are refused.
pub fn restore(store: &Store, session: &str) -> Result<()> {
    let _guard = admission_guard()?;
    restore_in_admission(store, session)
}
pub(crate) fn restore_in_admission(store: &Store, session: &str) -> Result<()> {
    validate_id(session)?;
    if store.data_dir().is_none() {
        return Ok(());
    }
    let data_dir = data_dir_of(store)?;
    store.session(session)?;
    ensure!(
        store.active_session_metadata(session)?.is_some(),
        "session {session} is still archived; restore its chat before restoring its worktree"
    );
    let Some(mirror) = read_mirror(&data_dir, session)? else {
        // A child with worktree:false still depends on its nearest isolated
        // ancestor, whose clean worktree may have been removed on archive.
        let mut current = store.session(session)?.parent_session_id;
        let mut seen = HashSet::new();
        while let Some(ancestor) = current {
            ensure!(
                seen.insert(ancestor.clone()),
                "session ancestry contains a cycle"
            );
            if read_mirror(&data_dir, &ancestor)?.is_some() {
                return restore_in_admission(store, &ancestor);
            }
            current = store.session(&ancestor)?.parent_session_id;
        }
        return Ok(());
    };
    validate_mirror(&data_dir, session, &mirror)?;
    let info = info_of(&mirror);
    if let Some(bound) = get(store, session)? {
        ensure!(
            bound == info,
            "attachment and binding mirror disagree for session {session}; refusing to restore"
        );
    }
    if mirror.state == MirrorState::Active {
        ensure!(
            occupied(&info.path),
            "worktree {} is missing but was not removed by cleanup; refusing to recreate it",
            info.path.display()
        );
        return Ok(());
    }
    if occupied(&info.path) {
        // A previous restore created the worktree but did not record it.
        ensure!(
            bound_worktree_listed(&info)?,
            "{} exists but is not the bound worktree; refusing to adopt it",
            info.path.display()
        );
        return write_mirror(&data_dir, session, &info, MirrorState::Active);
    }

    let repo = restore_repo(store, &data_dir, session, &info)?;
    ensure!(
        repo.is_dir(),
        "parent repository {} is missing; cannot recreate {}",
        repo.display(),
        info.path.display()
    );
    let path = utf8(&info.path)?;
    let full_branch = format!("refs/heads/{}", info.branch);
    if git_test(
        &repo,
        &["rev-parse", "--verify", "--quiet", full_branch.as_str()],
    )? {
        git(&repo, &["worktree", "add", path, info.branch.as_str()])?;
    } else {
        git(
            &repo,
            &["worktree", "add", "-b", info.branch.as_str(), path, "HEAD"],
        )?;
    }
    write_mirror(&data_dir, session, &info, MirrorState::Active)
}

/// Cleans every inactive session with a binding, deepest first. Archived and
/// deleted sessions are judged from their mirrors and the catalog only. Mirrors
/// already marked removed stay quiet, so repeated calls report only live problems.
pub fn cleanup_inactive(store: &Store) -> Result<Vec<CleanupReport>> {
    let _guard = admission_guard()?;
    cleanup_inactive_locked(store)
}
fn cleanup_inactive_locked(store: &Store) -> Result<Vec<CleanupReport>> {
    if store.data_dir().is_none() {
        return Ok(Vec::new());
    }
    let data_dir = data_dir_of(store)?;
    let mut reports = Vec::new();
    let mut mirrors = Vec::new();
    for (session, parsed) in read_mirror_files(&data_dir)? {
        match parsed {
            Ok(mirror) => mirrors.push((session, mirror)),
            Err(error) => reports.push(failed(
                &session,
                PathBuf::new(),
                String::new(),
                format!("binding mirror rejected: {error:#}"),
            )),
        }
    }

    let mut candidates = Vec::new();
    for (session, mirror) in &mirrors {
        if mirror.state == MirrorState::Removed || store.active_session_metadata(session)?.is_some()
        {
            continue;
        }
        candidates.push((session.clone(), mirror.clone()));
    }
    candidates.sort_by_key(|(_, mirror)| std::cmp::Reverse(depth(mirror, &mirrors)));

    for (session, mirror) in &candidates {
        if store.active_session_metadata(session)?.is_some() {
            continue;
        }
        match validate_mirror(&data_dir, session, mirror) {
            Ok(()) => reports.push(cleanup_one(&data_dir, session, &info_of(mirror))),
            Err(error) => reports.push(failed(
                session,
                mirror.path.clone(),
                mirror.branch.clone(),
                format!("binding rejected, nothing removed: {error:#}"),
            )),
        }
    }
    Ok(reports)
}

/// Archives the whole chat tree containing `session` (the store archives the root
/// even when given a child), then cleans inactive worktrees. Cleanup runs only
/// after a successful archive: a busy or already-archived tree may still be live,
/// and the store's catalog is the only proof that a session is inactive.
pub fn archive_chat(store: &Store, session: &str) -> Result<(bool, Vec<CleanupReport>)> {
    let _guard = admission_guard()?;
    let archived = store.archive_chat(session)?;
    let reports = if archived {
        cleanup_inactive_locked(store)?
    } else {
        Vec::new()
    };
    Ok((archived, reports))
}

/// Deletes the session in SQLite first. If that fails, the worktree is untouched.
pub fn delete_session(store: &Store, session: &str) -> Result<Vec<CleanupReport>> {
    let _guard = admission_guard()?;
    store.delete_session(session)?;
    cleanup_inactive_locked(store)
}

// Fixture-only removal exercises git checks against active session attachments.
// Production cleanup uses inactive catalog entries via archive/delete instead.
#[cfg(test)]
fn cleanup_subtree(store: &Store, session: &str) -> Result<Vec<CleanupReport>> {
    let _guard = admission_guard()?;
    let data_dir = data_dir_of(store)?;
    let sessions = store.list_sessions()?;
    let mut children: std::collections::HashMap<&str, Vec<&str>> = Default::default();
    for candidate in &sessions {
        if let Some(parent) = &candidate.parent_session_id {
            children
                .entry(parent.as_str())
                .or_default()
                .push(candidate.id.as_str());
        }
    }

    let mut subtree = Vec::new();
    let mut pending = vec![(session.to_owned(), 0_usize)];
    let mut seen = HashSet::new();
    while let Some((id, depth)) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        for child in children.get(id.as_str()).into_iter().flatten() {
            pending.push(((*child).to_owned(), depth + 1));
        }
        subtree.push((depth, id));
    }
    subtree.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));

    let mut reports = Vec::new();
    for (_, id) in &subtree {
        match get(store, id) {
            Ok(Some(info)) => reports.push(cleanup_one(&data_dir, id, &info)),
            Ok(None) => {}
            Err(error) => reports.push(failed(
                id,
                PathBuf::new(),
                String::new(),
                format!("binding is unreadable: {error:#}"),
            )),
        }
    }
    Ok(reports)
}

/// Decides what a failed attachment save means. Only a provably absent binding
/// rolls the new worktree back; anything uncertain leaves it for manual review.
fn settle_failed_save(
    store: &Store,
    data_dir: &Path,
    session: &str,
    parent: &Path,
    info: &WorktreeInfo,
    save_error: anyhow::Error,
) -> Result<()> {
    match get(store, session) {
        Ok(Some(saved)) if saved == *info => Ok(()),
        Ok(Some(saved)) => Err(save_error.context(format!(
            "session {session} gained another binding at {} meanwhile; the new worktree at {} and branch {} are left in place",
            saved.path.display(),
            info.path.display(),
            info.branch
        ))),
        Ok(None) => Err(abandon(
            parent,
            data_dir,
            session,
            info,
            save_error.context("could not save the worktree binding"),
        )),
        Err(read_error) => Err(save_error.context(format!(
            "could not save the worktree binding and could not confirm it ({read_error:#}); worktree {} and branch {} are left in place",
            info.path.display(),
            info.branch
        ))),
    }
}

/// Undoes a create that never became a usable binding. Both steps are
/// non-forcing, so git still refuses if anything changed in the meantime.
fn abandon(
    parent: &Path,
    data_dir: &Path,
    session: &str,
    info: &WorktreeInfo,
    error: anyhow::Error,
) -> anyhow::Error {
    match rollback(parent, info).and_then(|()| remove_mirror(data_dir, session)) {
        Ok(()) => error.context("the new worktree was removed"),
        Err(undo) => error.context(format!(
            "removing the new worktree failed ({undo:#}); remove {}, branch {} and its binding mirror by hand",
            info.path.display(),
            info.branch
        )),
    }
}

fn rollback(parent: &Path, info: &WorktreeInfo) -> Result<()> {
    git(parent, &["worktree", "remove", utf8(&info.path)?])?;
    git(parent, &["branch", "-d", info.branch.as_str()])?;
    Ok(())
}

/// Ancestor worktree to recreate from: the nearest ancestor with a binding, after
/// that binding has been restored, else the original parent workspace.
fn restore_repo(
    store: &Store,
    data_dir: &Path,
    session: &str,
    info: &WorktreeInfo,
) -> Result<PathBuf> {
    let mut seen = HashSet::new();
    let mut current = store.session(session)?.parent_session_id;
    while let Some(ancestor) = current.take() {
        ensure!(
            seen.insert(ancestor.clone()),
            "session ancestry of {session} contains a cycle"
        );
        match read_mirror(data_dir, &ancestor)? {
            Some(ancestor_mirror) => {
                restore_in_admission(store, &ancestor)?;
                return Ok(ancestor_mirror.path);
            }
            None => current = store.session(&ancestor)?.parent_session_id,
        }
    }
    Ok(info.parent_workspace.clone())
}

fn bound_worktree_listed(info: &WorktreeInfo) -> Result<bool> {
    let full_branch = format!("refs/heads/{}", info.branch);
    Ok(listed_worktrees(&info.parent_workspace)?
        .iter()
        .any(|entry| {
            same_path(&entry.path, &info.path)
                && entry.branch.as_deref() == Some(full_branch.as_str())
        }))
}

fn cleanup_one(data_dir: &Path, session: &str, info: &WorktreeInfo) -> CleanupReport {
    let (status, note) = match remove_checked(data_dir, session, info) {
        Ok(outcome) => outcome,
        Err(error) => (CleanupStatus::Failed, format!("{error:#}")),
    };
    CleanupReport {
        session_id: session.to_owned(),
        path: info.path.clone(),
        branch: info.branch.clone(),
        status,
        note,
    }
}

fn remove_checked(
    data_dir: &Path,
    session: &str,
    info: &WorktreeInfo,
) -> Result<(CleanupStatus, String)> {
    let name = info
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    ensure!(
        info.branch == branch_name(name),
        "binding branch {} does not match worktree path {}; retained",
        info.branch,
        info.path.display()
    );
    let parent = info.parent_workspace.as_path();
    if !parent.is_dir() {
        return Ok((
            CleanupStatus::Missing,
            format!(
                "parent workspace {} is missing or moved; worktree and branch {} retained",
                parent.display(),
                info.branch
            ),
        ));
    }
    if !occupied(&info.path) {
        return Ok((
            CleanupStatus::Missing,
            format!(
                "worktree {} is missing or moved; binding and branch {} retained; check git worktree list before pruning",
                info.path.display(),
                info.branch
            ),
        ));
    }

    let full_branch = format!("refs/heads/{}", info.branch);
    ensure!(
        !same_path(parent, &info.path),
        "refusing to clean the parent workspace itself"
    );
    let listed = listed_worktrees(parent)?;
    let Some(entry) = listed
        .iter()
        .find(|entry| same_path(&entry.path, &info.path))
    else {
        bail!(
            "{} is not a worktree of {}; retained",
            info.path.display(),
            parent.display()
        );
    };
    ensure!(
        entry.branch.as_deref() == Some(full_branch.as_str()),
        "worktree {} is not on branch {}; retained",
        info.path.display(),
        info.branch
    );

    let mut kept = Vec::new();
    if !git(
        &info.path,
        &[
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=all",
        ],
    )?
    .trim()
    .is_empty()
    {
        kept.push("uncommitted, untracked or ignored files");
    }
    if !git_test(
        parent,
        &["merge-base", "--is-ancestor", full_branch.as_str(), "HEAD"],
    )? {
        kept.push("commits that are not in the parent HEAD");
    }
    if !kept.is_empty() {
        return Ok((
            CleanupStatus::Retained,
            format!(
                "kept worktree {} and branch {}: {}",
                info.path.display(),
                info.branch,
                kept.join("; ")
            ),
        ));
    }

    // Keep the source workspace required by any retained or active descendant.
    for (child, mirror) in read_mirror_files(data_dir)? {
        let mirror = mirror?;
        if child != session && mirror.parent_workspace == info.path && occupied(&mirror.path) {
            return Ok((
                CleanupStatus::Retained,
                format!(
                    "kept worktree {} and branch {}: child {child} still depends on this workspace",
                    info.path.display(),
                    info.branch,
                ),
            ));
        }
    }
    git(parent, &["worktree", "remove", utf8(&info.path)?])?;
    // Record the removal before the branch step, so a later failure still leaves
    // the worktree restorable rather than reported as missing.
    let recorded = write_mirror(data_dir, session, info, MirrorState::Removed);
    let branch = git(parent, &["branch", "-d", info.branch.as_str()]);
    let mut problems = Vec::new();
    if let Err(error) = &recorded {
        problems.push(format!("binding mirror not updated: {error:#}"));
    }
    if let Err(error) = &branch {
        problems.push(format!("branch kept: {error:#}"));
    }
    if problems.is_empty() {
        return Ok((
            CleanupStatus::Removed,
            format!(
                "removed worktree {} and branch {}; restore recreates it from the mirror",
                info.path.display(),
                info.branch
            ),
        ));
    }
    Ok((
        CleanupStatus::Retained,
        format!(
            "removed worktree {} but {}",
            info.path.display(),
            problems.join("; ")
        ),
    ))
}

fn failed(session: &str, path: PathBuf, branch: String, note: String) -> CleanupReport {
    CleanupReport {
        session_id: session.to_owned(),
        path,
        branch,
        status: CleanupStatus::Failed,
        note,
    }
}

/// Number of parent-workspace links before a mirror reaches a non-worktree
/// parent. Children always have a larger depth than the worktree they start from.
fn depth(mirror: &Mirror, mirrors: &[(String, Mirror)]) -> usize {
    let mut depth = 0;
    let mut parent = mirror.parent_workspace.as_path();
    while let Some((_, outer)) = mirrors
        .iter()
        .find(|(_, outer)| same_path(&outer.path, parent))
    {
        depth += 1;
        if depth > mirrors.len() {
            break;
        }
        parent = outer.parent_workspace.as_path();
    }
    depth
}

fn info_of(mirror: &Mirror) -> WorktreeInfo {
    WorktreeInfo {
        path: mirror.path.clone(),
        branch: mirror.branch.clone(),
        parent_workspace: mirror.parent_workspace.clone(),
    }
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "session id {id:?} cannot name a worktree"
    );
    Ok(())
}

fn branch_name(session: &str) -> String {
    format!("{BRANCH_PREFIX}{session}")
}

/// Rejects a mirror that does not describe this session's managed location.
fn validate_mirror(data_dir: &Path, session: &str, mirror: &Mirror) -> Result<()> {
    validate_id(session)?;
    ensure!(
        mirror.session_id == session,
        "binding mirror names session {} instead of {session}",
        mirror.session_id
    );
    ensure!(
        mirror.branch == branch_name(session),
        "binding branch {} does not belong to session {session}",
        mirror.branch
    );
    let root = worktrees_root(data_dir);
    ensure!(
        !is_link(&root) && canonical(&root)? == root,
        "managed worktree directory {} is a symbolic link or resolves elsewhere",
        root.display()
    );
    let expected = root.join(session);
    ensure!(
        mirror.path == expected,
        "binding path {} is not the managed location {}",
        mirror.path.display(),
        expected.display()
    );
    if occupied(&mirror.path) {
        ensure!(
            !is_link(&mirror.path) && canonical(&mirror.path)? == expected,
            "worktree path {} is a symbolic link or resolves outside the managed location; refusing to touch it",
            mirror.path.display()
        );
    }
    Ok(())
}

/// Canonical data directory, so stored paths and the containment checks agree.
fn data_dir_of(store: &Store) -> Result<PathBuf> {
    let data_dir = store
        .data_dir()
        .ok_or_else(|| anyhow!("child worktrees need a store with a data directory"))?;
    canonical(data_dir)
}

fn worktrees_root(data_dir: &Path) -> PathBuf {
    data_dir.join("worktrees")
}

fn bindings_dir(data_dir: &Path) -> PathBuf {
    worktrees_root(data_dir).join(BINDINGS_DIR)
}

fn mirror_file(data_dir: &Path, session: &str) -> PathBuf {
    bindings_dir(data_dir).join(format!("{session}.json"))
}

fn occupied(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn is_link(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
}

fn read_mirror(data_dir: &Path, session: &str) -> Result<Option<Mirror>> {
    validate_id(session)?;
    let file = mirror_file(data_dir, session);
    if !occupied(&file) {
        return Ok(None);
    }
    ensure!(
        !is_link(&file),
        "binding mirror {} is a symbolic link",
        file.display()
    );
    let bytes = fs::read(&file)
        .with_context(|| format!("could not read binding mirror {}", file.display()))?;
    let mirror = serde_json::from_slice(&bytes)
        .with_context(|| format!("binding mirror {} is unreadable", file.display()))?;
    Ok(Some(mirror))
}

/// Every well-formed mirror file name in the bindings directory, with its parse result.
fn read_mirror_files(data_dir: &Path) -> Result<Vec<(String, Result<Mirror>)>> {
    let dir = bindings_dir(data_dir);
    if !occupied(&dir) {
        return Ok(Vec::new());
    }
    ensure!(
        !is_link(&dir),
        "binding directory {} is a symbolic link",
        dir.display()
    );
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let name = entry?.file_name();
        let Some(session) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        if validate_id(session).is_err() {
            continue;
        }
        let mirror = read_mirror(data_dir, session)
            .and_then(|found| found.ok_or_else(|| anyhow!("binding mirror {session} disappeared")));
        found.push((session.to_owned(), mirror));
    }
    Ok(found)
}

/// Writes the mirror through a temporary file and a rename, so readers see the
/// old or the new contents, never a partial file.
fn write_mirror(
    data_dir: &Path,
    session: &str,
    info: &WorktreeInfo,
    state: MirrorState,
) -> Result<()> {
    validate_id(session)?;
    let dir = bindings_dir(data_dir);
    ensure!(
        !is_link(&dir),
        "binding directory {} is a symbolic link",
        dir.display()
    );
    fs::create_dir_all(&dir)?;
    let mirror = Mirror {
        session_id: session.to_owned(),
        path: info.path.clone(),
        branch: info.branch.clone(),
        parent_workspace: info.parent_workspace.clone(),
        state,
    };
    let bytes = serde_json::to_vec_pretty(&mirror)?;
    let target = mirror_file(data_dir, session);
    let temp = dir.join(format!(".{session}.{}.tmp", uuid::Uuid::new_v4()));
    if let Err(error) = write_atomic(&temp, &target, &bytes) {
        let _ = fs::remove_file(&temp);
        return Err(error)
            .with_context(|| format!("could not write binding mirror {}", target.display()));
    }
    Ok(())
}

fn write_atomic(temp: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, target)
}

fn remove_mirror(data_dir: &Path, session: &str) -> Result<()> {
    validate_id(session)?;
    match fs::remove_file(mirror_file(data_dir, session)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Plain view of a path. Windows verbatim prefixes are dropped, since git
/// does not accept them.
fn plain(path: &Path) -> PathBuf {
    if !cfg!(windows) {
        return path.to_path_buf();
    }
    let text = path.to_string_lossy();
    match (text.strip_prefix(r"\\?\UNC\"), text.strip_prefix(r"\\?\")) {
        (Some(unc), _) => PathBuf::from(format!(r"\\{unc}")),
        (None, Some(local)) => PathBuf::from(local),
        (None, None) => path.to_path_buf(),
    }
}

fn canonical(path: &Path) -> Result<PathBuf> {
    Ok(plain(&path.canonicalize()?))
}

fn same_path(left: &Path, right: &Path) -> bool {
    matches!((canonical(left), canonical(right)), (Ok(left), Ok(right)) if left == right)
}

struct Listed {
    path: PathBuf,
    branch: Option<String>,
}

fn listed_worktrees(parent: &Path) -> Result<Vec<Listed>> {
    let text = git(parent, &["worktree", "list", "--porcelain"])?;
    let mut listed: Vec<Listed> = Vec::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            listed.push(Listed {
                path: PathBuf::from(path),
                branch: None,
            });
        } else if let (Some(branch), Some(last)) = (line.strip_prefix("branch "), listed.last_mut())
        {
            last.branch = Some(branch.to_owned());
        }
    }
    Ok(listed)
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("path {} is not valid UTF-8", path.display()))
}

fn run(dir: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| {
            format!(
                "could not start git in {}; is git installed?",
                dir.display()
            )
        })
}

/// Runs git and returns stdout; a nonzero exit is an error that carries stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = run(dir, args)?;
    ensure!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        dir.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// For probes where exit 0 means yes and exit 1 means no, as with
/// `rev-parse --verify --quiet` and `merge-base --is-ancestor`.
fn git_test(dir: &Path, args: &[&str]) -> Result<bool> {
    let output = run(dir, args)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::Session;
    use tempfile::TempDir;

    fn store() -> Result<(TempDir, Store)> {
        let data = tempfile::tempdir()?;
        let store = Store::open_data_dir(data.path())?;
        Ok((data, store))
    }

    fn commit(dir: &Path, message: &str) -> Result<()> {
        git(dir, &["add", "-A"])?;
        git(
            dir,
            &[
                "-c",
                "user.name=BubblePaw Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                message,
            ],
        )?;
        Ok(())
    }

    /// A throwaway repository with one commit that ignores `*.log`.
    fn repo_with_commit() -> Result<TempDir> {
        let repo = tempfile::tempdir()?;
        git(repo.path(), &["init", "-q"])?;
        fs::write(repo.path().join(".gitignore"), "*.log\n")?;
        fs::write(repo.path().join("README.md"), "parent\n")?;
        commit(repo.path(), "initial")?;
        Ok(repo)
    }

    fn head(dir: &Path) -> Result<String> {
        Ok(git(dir, &["rev-parse", "HEAD"])?.trim().to_owned())
    }

    fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
        let full = format!("refs/heads/{branch}");
        git_test(repo, &["rev-parse", "--verify", "--quiet", full.as_str()])
    }

    fn mirror_of(store: &Store, session: &str) -> Result<Mirror> {
        read_mirror(&data_dir_of(store)?, session)?.context("no binding mirror")
    }

    fn child_of(store: &Store, parent: &str) -> Result<Session> {
        subchat(store, "child", parent)
    }

    fn subchat(store: &Store, title: &str, parent: &str) -> Result<Session> {
        store.create_session_with_parent(title, Default::default(), Some(parent))
    }

    #[test]
    fn create_binds_branch_from_parent_head_and_skips_uncommitted_parent_changes() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        fs::write(repo.path().join("draft.txt"), "uncommitted")?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let parent_head = head(repo.path())?;

        let info = create(&store, repo.path(), &child.id)?;

        let data_dir = data_dir_of(&store)?;
        assert_eq!(info.path, data_dir.join("worktrees").join(&child.id));
        assert_eq!(info.branch, format!("bp/{}", child.id));
        assert_eq!(info.parent_workspace, canonical(repo.path())?);
        assert!(info.path.join("README.md").is_file());
        assert!(!info.path.join("draft.txt").exists());
        assert_eq!(head(&info.path)?, parent_head);
        assert_eq!(get(&store, &child.id)?, Some(info.clone()));
        assert_eq!(mirror_of(&store, &child.id)?.state, MirrorState::Active);
        assert!(create(&store, repo.path(), &child.id).is_err());
        Ok(())
    }

    #[test]
    fn clean_subtree_removes_clean_worktrees_deepest_first() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let child_info = create(&store, repo.path(), &child.id)?;
        let grand = subchat(&store, "grand", &child.id)?;
        let grand_info = create(&store, &child_info.path, &grand.id)?;

        let reports = cleanup_subtree(&store, &parent.id)?;

        let order: Vec<&str> = reports
            .iter()
            .map(|report| report.session_id.as_str())
            .collect();
        assert_eq!(order, [grand.id.as_str(), child.id.as_str()]);
        assert!(
            reports
                .iter()
                .all(|report| report.status == CleanupStatus::Removed)
        );
        assert!(!grand_info.path.exists());
        assert!(!child_info.path.exists());
        assert!(!branch_exists(repo.path(), &grand_info.branch)?);
        assert!(!branch_exists(repo.path(), &child_info.branch)?);
        let json = serde_json::to_value(&reports[0])?;
        assert_eq!(json["status"], "removed");
        assert!(json.get("sessionId").is_some());
        Ok(())
    }

    #[test]
    fn dirty_untracked_and_ignored_files_are_retained() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let info = create(&store, repo.path(), &child.id)?;

        fs::write(info.path.join("notes.txt"), "untracked")?;
        let reports = cleanup_subtree(&store, &parent.id)?;
        assert_eq!(reports[0].status, CleanupStatus::Retained);
        assert!(info.path.join("notes.txt").exists());

        fs::remove_file(info.path.join("notes.txt"))?;
        fs::write(info.path.join("build.log"), "ignored")?;
        let reports = cleanup_subtree(&store, &parent.id)?;
        assert_eq!(reports[0].status, CleanupStatus::Retained);
        assert!(info.path.join("build.log").exists());
        assert!(branch_exists(repo.path(), &info.branch)?);

        fs::remove_file(info.path.join("build.log"))?;
        let reports = cleanup_subtree(&store, &parent.id)?;
        assert_eq!(reports[0].status, CleanupStatus::Removed);
        Ok(())
    }

    #[test]
    fn unmerged_commits_are_retained_until_the_parent_contains_them() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let info = create(&store, repo.path(), &child.id)?;
        fs::write(info.path.join("child.txt"), "work")?;
        commit(&info.path, "child work")?;

        let reports = cleanup_subtree(&store, &child.id)?;
        assert_eq!(reports[0].status, CleanupStatus::Retained);
        assert!(reports[0].note.contains("parent HEAD"));
        assert!(info.path.join("child.txt").exists());
        assert!(branch_exists(repo.path(), &info.branch)?);

        git(repo.path(), &["merge", "--ff-only", info.branch.as_str()])?;
        let reports = cleanup_subtree(&store, &child.id)?;
        assert_eq!(reports[0].status, CleanupStatus::Removed);
        assert!(!branch_exists(repo.path(), &info.branch)?);
        Ok(())
    }

    #[test]
    fn authoritative_root_workspace_precedes_host_fallback_and_children_use_worktrees() -> Result<()>
    {
        let (_data, store) = store()?;
        let host = repo_with_commit()?;
        let client = repo_with_commit()?;
        store.bind_workspace(host.path())?;
        let root = store.create_session_with_workspace("client", client.path())?;
        let child = child_of(&store, &root.id)?;
        assert_eq!(
            workspace(&store, &root.id, host.path())?,
            canonical(client.path())?
        );
        assert_eq!(
            workspace(&store, &child.id, host.path())?,
            canonical(client.path())?
        );
        let info = create(&store, client.path(), &child.id)?;
        let grandchild = subchat(&store, "grandchild", &child.id)?;
        assert_eq!(workspace(&store, &child.id, host.path())?, info.path);
        assert_eq!(workspace(&store, &grandchild.id, host.path())?, info.path);
        assert_eq!(
            store.session_workspace(&grandchild.id)?,
            Some(client.path().canonicalize()?)
        );
        Ok(())
    }

    #[test]
    fn grandchild_starts_from_child_head_and_workspace_walks_ancestors() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let child_info = create(&store, repo.path(), &child.id)?;
        fs::write(child_info.path.join("child.txt"), "work")?;
        commit(&child_info.path, "child work")?;
        let child_head = head(&child_info.path)?;

        let grand = subchat(&store, "grand", &child.id)?;
        let grand_info = create(&store, &child_info.path, &grand.id)?;
        assert_eq!(grand_info.parent_workspace, child_info.path);
        assert_eq!(head(&grand_info.path)?, child_head);
        assert!(grand_info.path.join("child.txt").exists());

        assert_eq!(workspace(&store, &grand.id, repo.path())?, grand_info.path);
        let sibling = subchat(&store, "sibling", &child.id)?;
        assert_eq!(
            workspace(&store, &sibling.id, repo.path())?,
            child_info.path
        );
        assert_eq!(workspace(&store, &parent.id, repo.path())?, repo.path());

        // The grandchild's branch is already contained in the child's HEAD, so only
        // the child, whose commit is not in the parent HEAD, is retained.
        let reports = cleanup_subtree(&store, &child.id)?;
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].session_id, grand.id);
        assert_eq!(reports[0].status, CleanupStatus::Removed);
        assert_eq!(reports[1].session_id, child.id);
        assert_eq!(reports[1].status, CleanupStatus::Retained);
        Ok(())
    }

    #[test]
    fn missing_worktree_is_reported_and_its_branch_kept() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let info = create(&store, repo.path(), &child.id)?;
        fs::remove_dir_all(&info.path)?;

        let reports = cleanup_subtree(&store, &parent.id)?;

        assert_eq!(reports[0].status, CleanupStatus::Missing);
        assert!(branch_exists(repo.path(), &info.branch)?);
        assert!(workspace(&store, &child.id, repo.path()).is_err());
        Ok(())
    }

    #[test]
    fn non_git_parent_fails_clearly_and_binds_nothing() -> Result<()> {
        let (_data, store) = store()?;
        let plain_dir = tempfile::tempdir()?;
        let child = store.create_session("child")?;

        let error =
            create(&store, plain_dir.path(), &child.id).expect_err("a non-git parent must fail");

        assert!(
            format!("{error:#}").contains("not a git repository"),
            "{error:#}"
        );
        assert!(get(&store, &child.id)?.is_none());
        assert!(read_mirror(&data_dir_of(&store)?, &child.id)?.is_none());
        Ok(())
    }

    #[test]
    fn archive_wrapper_keeps_archive_state_and_restore_recreates_the_worktree() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let info = create(&store, repo.path(), &child.id)?;
        let parent_head = head(repo.path())?;

        // Archiving through a child archives the whole root tree.
        let (archived, reports) = archive_chat(&store, &child.id)?;

        assert!(archived);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].status, CleanupStatus::Removed);
        assert!(!info.path.exists());
        assert!(store.active_session_metadata(&child.id)?.is_none());
        assert!(store.active_session_metadata(&parent.id)?.is_none());
        assert!(
            cleanup_inactive(&store)?.is_empty(),
            "removed records stay quiet"
        );

        restore(&store, &child.id)?;

        assert!(store.active_session_metadata(&child.id)?.is_some());
        assert_eq!(get(&store, &child.id)?, Some(info.clone()));
        assert!(info.path.join("README.md").is_file());
        assert_eq!(head(&info.path)?, parent_head);
        assert_eq!(workspace(&store, &child.id, repo.path())?, info.path);
        assert!(cleanup_inactive(&store)?.is_empty());
        Ok(())
    }

    #[test]
    fn restore_walks_ancestry_and_recreates_the_parent_worktree_first() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let child_info = create(&store, repo.path(), &child.id)?;
        let grand = subchat(&store, "grand", &child.id)?;
        let grand_info = create(&store, &child_info.path, &grand.id)?;

        let (_, reports) = archive_chat(&store, &grand.id)?;
        assert!(
            reports
                .iter()
                .all(|report| report.status == CleanupStatus::Removed)
        );
        assert!(!child_info.path.exists());
        assert!(!grand_info.path.exists());

        restore(&store, &grand.id)?;

        assert!(child_info.path.join("README.md").is_file());
        assert_eq!(workspace(&store, &grand.id, repo.path())?, grand_info.path);
        assert_eq!(head(&grand_info.path)?, head(&child_info.path)?);
        Ok(())
    }

    #[test]
    fn restore_refuses_an_unexpected_missing_worktree() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let info = create(&store, repo.path(), &child.id)?;
        fs::remove_dir_all(&info.path)?;

        assert!(restore(&store, &child.id).is_err());

        assert!(!info.path.exists());
        assert_eq!(mirror_of(&store, &child.id)?.state, MirrorState::Active);
        assert!(workspace(&store, &child.id, repo.path()).is_err());
        Ok(())
    }

    #[test]
    fn delete_wrapper_leaves_the_worktree_when_deletion_fails() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let info = create(&store, repo.path(), &child.id)?;

        assert!(delete_session(&store, "unknown-session").is_err());

        assert!(info.path.join("README.md").is_file());
        assert_eq!(get(&store, &child.id)?, Some(info.clone()));
        assert_eq!(mirror_of(&store, &child.id)?.state, MirrorState::Active);
        assert!(
            cleanup_inactive(&store)?.is_empty(),
            "an active child is never cleaned"
        );
        assert!(info.path.exists());

        let reports = delete_session(&store, &child.id)?;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].status, CleanupStatus::Removed);
        assert!(!info.path.exists());
        assert!(store.active_session_metadata(&child.id)?.is_none());
        Ok(())
    }

    #[test]
    fn dirty_archived_worktree_is_retained_and_reported_each_time() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let info = create(&store, repo.path(), &child.id)?;
        fs::write(info.path.join("notes.txt"), "keep me")?;

        let (_, reports) = archive_chat(&store, &child.id)?;
        assert_eq!(reports[0].status, CleanupStatus::Retained);
        assert!(info.path.join("notes.txt").exists());
        assert_eq!(mirror_of(&store, &child.id)?.state, MirrorState::Active);
        assert_eq!(cleanup_inactive(&store)?[0].status, CleanupStatus::Retained);
        Ok(())
    }

    #[test]
    fn tampered_mirror_path_is_rejected_and_nothing_is_removed() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let info = create(&store, repo.path(), &child.id)?;
        let file = mirror_file(&data_dir_of(&store)?, &child.id);
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&file)?)?;
        value["path"] = serde_json::Value::String(repo.path().display().to_string());
        fs::write(&file, serde_json::to_vec(&value)?)?;

        let (_, reports) = archive_chat(&store, &child.id)?;

        assert_eq!(reports[0].status, CleanupStatus::Failed);
        assert!(repo.path().join("README.md").is_file());
        assert!(info.path.join("README.md").is_file());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_worktree_path_is_rejected_and_not_followed() -> Result<()> {
        let (_data, store) = store()?;
        let repo = repo_with_commit()?;
        let parent = store.create_session("parent")?;
        let child = child_of(&store, &parent.id)?;
        let info = create(&store, repo.path(), &child.id)?;
        fs::remove_dir_all(&info.path)?;
        std::os::unix::fs::symlink(repo.path(), &info.path)?;

        let (_, reports) = archive_chat(&store, &child.id)?;

        assert_eq!(reports[0].status, CleanupStatus::Failed);
        assert!(repo.path().join("README.md").is_file());
        assert!(repo.path().join(".git").exists());
        Ok(())
    }
}
