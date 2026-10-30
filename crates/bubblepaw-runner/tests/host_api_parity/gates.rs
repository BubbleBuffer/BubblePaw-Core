//! Profile gates, checked at the dispatch the transports share. A method above
//! the caller's profile is refused with 403 before its parameters are read or
//! its handler runs, so a refused write must leave the store as it was.
use bubblepaw_core::Store;
use bubblepaw_runner::host_api::{HostCtx, METHODS, Profile, dispatch};
use serde_json::json;
use std::path::Path;

const DENIED: i32 = -32003;

fn context(root: &Path) -> (HostCtx, Store) {
    let store = Store::open_data_dir(root).unwrap();
    (HostCtx::for_store(store.clone(), root.to_path_buf()), store)
}

/// Every table method above the caller's profile is denied with no parameters.
/// The check runs before parameter decoding, so `{}` is enough to reach it.
#[tokio::test]
async fn methods_above_the_caller_profile_are_denied() {
    let root = tempfile::tempdir().unwrap();
    let (ctx, _store) = context(root.path());
    for caller in [Profile::Read, Profile::Edit] {
        for method in METHODS.iter().filter(|method| caller < method.profile) {
            let error = dispatch(&ctx, caller, method.name, json!({}))
                .await
                .expect_err(method.name);
            assert_eq!(error.rpc_code(), DENIED, "{} for {caller:?}", method.name);
            assert_eq!(error.http_status(), 403, "{} for {caller:?}", method.name);
        }
    }
}

/// A Read caller is never refused a Read method. Errors from invalid parameters
/// are fine here; only the profile refusal is not.
#[tokio::test]
async fn read_methods_are_not_denied_to_a_read_caller() {
    let root = tempfile::tempdir().unwrap();
    let (ctx, _store) = context(root.path());
    for method in METHODS
        .iter()
        .filter(|method| method.profile == Profile::Read)
    {
        if let Err(error) = dispatch(&ctx, Profile::Read, method.name, json!({})).await {
            assert_ne!(
                error.rpc_code(),
                DENIED,
                "{} was denied to a Read caller",
                method.name
            );
        }
    }
}

/// Writes above the caller's profile are refused and change nothing: the
/// session row stays as it was and still exists.
#[tokio::test]
async fn refused_writes_leave_the_store_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let (ctx, store) = context(root.path());
    let session = store.create_session("parity gate").unwrap();
    let before = serde_json::to_value(store.session(&session.id).unwrap()).unwrap();
    let refused = [
        (
            Profile::Read,
            "session.rename",
            json!({"session_id": session.id, "title": "parity gate renamed"}),
        ),
        (
            Profile::Read,
            "session.archive",
            json!({"session_id": session.id}),
        ),
        (
            Profile::Edit,
            "session.delete",
            json!({"session_id": session.id}),
        ),
        (Profile::Edit, "providers.remove", json!({"name": "demo"})),
        (
            Profile::Edit,
            "environment.control",
            json!({"session_id": session.id, "control": {}}),
        ),
        (Profile::Edit, "shutdown", json!({})),
    ];
    for (caller, method, params) in refused {
        let error = dispatch(&ctx, caller, method, params)
            .await
            .expect_err(method);
        assert_eq!(error.rpc_code(), DENIED, "{method} for {caller:?}");
        assert_eq!(error.http_status(), 403, "{method} for {caller:?}");
    }
    let after = serde_json::to_value(store.session(&session.id).unwrap()).unwrap();
    assert_eq!(before, after, "a refused write changed the session row");
}
