//! The configured account a provider call is billed to, scoped to the task.
use std::future::Future;

tokio::task_local! {
    static ACCOUNT: String;
}

/// Bind physical attempts to a configured account without changing provider routing.
/// Nested scopes restore their caller's account; concurrent tasks remain isolated.
pub async fn with_request_usage_account<F: Future>(account: &str, future: F) -> F::Output {
    ACCOUNT.scope(account.to_owned(), future).await
}

/// The account of the active scope, or None outside one.
pub fn current_account() -> Option<String> {
    ACCOUNT.try_with(Clone::clone).ok()
}
