use anyhow::Result;
use bubblepaw_core::{ContextStrategy, Store};
use bubblepaw_runner::{scratchpads::ScratchpadStore, tools::WorkspaceTools};
use std::{fs, path::Path};

#[test]
fn root_chat_and_descendants_share_files_until_root_deletion() -> Result<()> {
    for persistent in [false, true] {
        let data = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let store = if persistent {
            Store::open_data_dir(data.path())?
        } else {
            Store::in_memory()?
        };
        let root = store.create_session("root")?;
        let child = store.create_session_with_parent(
            "child",
            ContextStrategy::default(),
            Some(&root.id),
        )?;
        let grandchild = store.create_session_with_parent(
            "grandchild",
            ContextStrategy::default(),
            Some(&child.id),
        )?;
        let service = ScratchpadStore::new(data.path())?;
        let roots = service.for_session(&store, &root.id, workspace.path())?;
        let nested = service.for_session(&store, &grandchild.id, workspace.path())?;
        assert_eq!(roots.chat, nested.chat);
        assert_eq!(roots.workspace, nested.workspace);
        assert_eq!(
            roots.chat,
            data.path()
                .canonicalize()?
                .join("scratch/chat")
                .join(&root.id)
        );
        fs::write(roots.chat.join("script.txt"), "shared")?;
        store.delete_session_preserving_effects(&child.id)?;
        assert_eq!(fs::read_to_string(roots.chat.join("script.txt"))?, "shared");
        store.delete_session(&root.id)?;
        assert!(!roots.chat.exists());
        assert!(
            roots.workspace.exists(),
            "workspace scratch survives chat deletion"
        );
    }
    Ok(())
}

#[test]
fn workspace_identity_is_stable_across_instances_and_chat_identity_is_private() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let other_workspace = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let one = store.create_session("one")?;
    let two = store.create_session("two")?;
    let first =
        ScratchpadStore::new(data.path())?.for_session(&store, &one.id, workspace.path())?;
    let second = ScratchpadStore::new(data.path())?.for_session(
        &store,
        &two.id,
        workspace.path().join("."),
    )?;
    let third =
        ScratchpadStore::new(data.path())?.for_session(&store, &one.id, other_workspace.path())?;
    assert_eq!(first.workspace, second.workspace);
    assert_ne!(first.chat, second.chat);
    assert_ne!(first.workspace, third.workspace);
    assert_eq!(first.chat, third.chat);
    assert!(
        first
            .resolve(&second.chat.join("private.txt"), true)
            .is_err()
    );
    assert!(
        first
            .resolve(&third.workspace.join("private.txt"), true)
            .is_err()
    );
    Ok(())
}

#[test]
fn root_delete_cleanup_can_be_registered_after_reopening() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let service = ScratchpadStore::new(data.path())?;
    let (id, path) = {
        let store = Store::open_data_dir(data.path())?;
        let session = store.create_session("reopen")?;
        let paths = service.for_session(&store, &session.id, workspace.path())?;
        fs::write(paths.chat.join("file"), "survives restart")?;
        (session.id, paths.chat)
    };
    let store = Store::open_data_dir(data.path())?;
    service.register_cleanup(&store);
    store.delete_session(&id)?;
    assert!(!path.exists());
    Ok(())
}

#[test]
fn scratch_resolution_rejects_parent_components_and_siblings_before_workspace_fallback()
-> Result<()> {
    // Data inside the workspace must not make another chat's scratch writable.
    let workspace = tempfile::tempdir()?;
    let data = workspace.path().join("data");
    let store = Store::in_memory()?;
    let session = store.create_session("resolution")?;
    let service = ScratchpadStore::new(&data)?;
    let paths = service.for_session(&store, &session.id, workspace.path())?;
    let missing = paths.chat.join("nested/new.txt");
    assert_eq!(paths.resolve(&missing, true)?, Some(missing.clone()));
    assert!(paths.resolve(&missing, false).is_err());
    fs::write(paths.chat.join("file"), "text")?;
    for path in [
        paths.chat.join("../file"),
        paths.workspace.join("../file"),
        paths.chat.join("sub/../../file"),
    ] {
        assert!(paths.resolve(&path, true).is_err(), "{}", path.display());
    }
    let other = data.canonicalize()?.join("scratch/chat/another/file");
    assert!(paths.resolve(&other, true).is_err());
    let relative = paths.chat.strip_prefix(workspace.path().canonicalize()?)?;
    let traversal = format!(
        "{}/../{}/file",
        relative.display(),
        paths.chat.file_name().unwrap().to_string_lossy()
    );
    assert!(
        paths
            .resolve_from(&workspace.path().canonicalize()?, &traversal, true)
            .is_err()
    );
    assert_eq!(
        paths.resolve(&workspace.path().join("ordinary"), true)?,
        None
    );
    Ok(())
}

#[test]
fn configured_data_dir_wins_for_in_memory_dispatchers_and_unconfigured_hosts_get_paths()
-> Result<()> {
    let workspace = tempfile::tempdir()?;
    let configured = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("embedder")?;
    let tools = WorkspaceTools::new(workspace.path())?;
    let first = tools.scratchpads(&store, &session.id)?;
    let second = tools.clone().scratchpads(&store, &session.id)?;
    assert_eq!(first.chat, second.chat);
    assert!(first.chat.is_dir() && first.workspace.is_dir());
    tools.prepare_scratchpads(configured.path())?;
    let paths = tools.scratchpads(&store, &session.id)?;
    assert!(paths.chat.starts_with(configured.path().canonicalize()?));
    assert_ne!(first.chat, paths.chat);
    let separate = WorkspaceTools::new(workspace.path())?;
    separate.prepare_scratchpads(configured.path())?;
    assert_eq!(paths.chat, separate.scratchpads(&store, &session.id)?.chat);
    Ok(())
}

#[test]
fn deleting_after_a_temporary_host_drops_does_not_recreate_its_directory() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("temporary lifetime")?;
    let data = {
        let tools = WorkspaceTools::new(workspace.path())?;
        let paths = tools.scratchpads(&store, &session.id)?;
        paths
            .chat
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned()
    };
    assert!(
        !data.exists(),
        "the owned temporary host directory has been released"
    );
    store.delete_session(&session.id)?;
    assert!(
        !data.exists(),
        "a cleanup callback must not recreate released temporary data"
    );
    Ok(())
}

fn reject_link_escape(link: impl Fn(&Path, &Path)) -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("link")?;
    let service = ScratchpadStore::new(data.path())?;
    let paths = service.for_session(&store, &session.id, workspace.path())?;
    fs::write(workspace.path().join("private.txt"), "workspace file")?;
    for root in [&paths.chat, &paths.workspace] {
        link(&root.join("escape"), workspace.path());
        assert!(
            paths
                .resolve(&root.join("escape/private.txt"), false)
                .is_err()
        );
        assert!(
            paths
                .resolve(&root.join("escape/new/nested.txt"), true)
                .is_err()
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn scratch_symlinks_cannot_escape_to_the_workspace() -> Result<()> {
    reject_link_escape(|link, target| std::os::unix::fs::symlink(target, link).unwrap())
}

#[cfg(windows)]
#[test]
fn scratch_junctions_cannot_escape_to_the_workspace() -> Result<()> {
    reject_link_escape(|link, target| {
        let plain = |path: &Path| {
            path.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_owned()
        };
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(plain(link))
            .arg(plain(target))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    })
}

#[cfg(windows)]
#[test]
fn ordinary_windows_absolute_paths_and_verbatim_paths_share_the_same_grant() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("Windows paths")?;
    let service = ScratchpadStore::new(data.path())?;
    let paths = service.for_session(&store, &session.id, workspace.path())?;
    for root in [&paths.chat, &paths.workspace] {
        let target = root.join("file.txt");
        fs::write(&target, "windows")?;
        let ordinary = target
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned();
        assert_eq!(
            paths.resolve_from(workspace.path(), &ordinary, false)?,
            Some(target.clone())
        );
        assert_eq!(
            paths.resolve_from(workspace.path(), &ordinary.to_ascii_uppercase(), false)?,
            Some(target.clone())
        );
        let traversal = format!(
            r"{}\..\{}\file.txt",
            root.display(),
            root.file_name().unwrap().to_string_lossy()
        );
        assert!(
            paths
                .resolve_from(workspace.path(), &traversal, false)
                .is_err(),
            "verbatim paths must retain .. checks"
        );
    }
    Ok(())
}
