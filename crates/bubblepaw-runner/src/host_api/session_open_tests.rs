//! `session.open` with a `workspace`: tunnel callers open only known workspaces.
use super::*;
use anyhow::Result;

async fn open_at(ctx: &HostCtx, path: &std::path::Path) -> Result<Value, HostError> {
    dispatch(
        ctx,
        Profile::Edit,
        "session.open",
        json!({"title":"at","workspace":path}),
    )
    .await
}

fn bound(store: &Store, session: &Value) -> Result<PathBuf> {
    Ok(store
        .session_workspace(session["id"].as_str().expect("session id"))?
        .expect("session workspace"))
}

fn tunnel(store: Store, default: &std::path::Path) -> HostCtx {
    let mut ctx = HostCtx::for_store(store, default);
    ctx.read_scope = ReadScope::Host;
    ctx.known_workspaces_only = true;
    ctx
}

#[tokio::test]
async fn tunnel_opens_a_workspace_an_existing_session_uses() -> Result<()> {
    let default = tempfile::tempdir()?;
    let known = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    store.create_session_with_workspace("existing", known.path())?;
    let ctx = tunnel(store.clone(), default.path());
    let opened = open_at(&ctx, known.path()).await?;
    assert_eq!(bound(&store, &opened)?, known.path().canonicalize()?);
    Ok(())
}

#[tokio::test]
async fn tunnel_accepts_the_host_default_workspace() -> Result<()> {
    let default = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let ctx = tunnel(store.clone(), default.path());
    let opened = open_at(&ctx, default.path()).await?;
    assert_eq!(bound(&store, &opened)?, default.path().canonicalize()?);
    Ok(())
}

#[tokio::test]
async fn tunnel_refuses_an_unknown_workspace_with_the_typed_error() -> Result<()> {
    let default = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let ctx = tunnel(store.clone(), default.path());
    let error = open_at(&ctx, other.path()).await.unwrap_err();
    assert_eq!((error.http_status(), error.rpc_code()), (403, -32003));
    assert!(error.message.contains(&other.path().display().to_string()));
    assert!(error.message.contains("does not know"), "{}", error.message);
    assert_eq!(
        error.data.as_ref().unwrap()["workspace"],
        other.path().display().to_string()
    );
    // A path that does not exist is refused the same way, and nothing is created.
    let missing = other.path().join("nope");
    let error = open_at(&ctx, &missing).await.unwrap_err();
    assert_eq!(error.rpc_code(), -32003);
    assert!(store.list_sessions()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn tunnel_refuses_traversal_out_of_a_known_workspace() -> Result<()> {
    let default = tempfile::tempdir()?;
    let known = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    store.create_session_with_workspace("existing", known.path())?;
    let ctx = tunnel(store.clone(), default.path());
    // A child of a known workspace is a different path, so it is unknown.
    let child = known.path().join("child");
    std::fs::create_dir(&child)?;
    assert_eq!(open_at(&ctx, &child).await.unwrap_err().rpc_code(), -32003);
    // `..` that lands back on a known path is that path; one that leaves is not.
    let back = child.join("..");
    let opened = open_at(&ctx, &back).await?;
    assert_eq!(bound(&store, &opened)?, known.path().canonicalize()?);
    let out = known.path().join("..");
    assert_eq!(open_at(&ctx, &out).await.unwrap_err().rpc_code(), -32003);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn tunnel_follows_a_symlink_to_the_real_path() -> Result<()> {
    let default = tempfile::tempdir()?;
    let known = tempfile::tempdir()?;
    let secret = tempfile::tempdir()?;
    let links = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    store.create_session_with_workspace("existing", known.path())?;
    let ctx = tunnel(store.clone(), default.path());
    // A link to an unknown directory is refused; a link to a known one resolves.
    std::os::unix::fs::symlink(secret.path(), links.path().join("secret"))?;
    std::os::unix::fs::symlink(known.path(), links.path().join("known"))?;
    let error = open_at(&ctx, &links.path().join("secret"))
        .await
        .unwrap_err();
    assert_eq!(error.rpc_code(), -32003);
    let opened = open_at(&ctx, &links.path().join("known")).await?;
    assert_eq!(bound(&store, &opened)?, known.path().canonicalize()?);
    Ok(())
}

#[tokio::test]
async fn local_caller_opens_any_workspace() -> Result<()> {
    let default = tempfile::tempdir()?;
    let elsewhere = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let ctx = HostCtx::for_store(store.clone(), default.path());
    let opened = open_at(&ctx, elsewhere.path()).await?;
    assert_eq!(bound(&store, &opened)?, elsewhere.path().canonicalize()?);
    let error = open_at(&ctx, &elsewhere.path().join("nope"))
        .await
        .unwrap_err();
    assert_eq!(error.http_status(), 400);
    // Absent, the session still takes the host's workspace.
    let plain = dispatch(&ctx, Profile::Edit, "session.open", json!({})).await?;
    assert_eq!(bound(&store, &plain)?, default.path().canonicalize()?);
    Ok(())
}
