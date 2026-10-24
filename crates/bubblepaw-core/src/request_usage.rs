//! Small authoritative billing records, separate from disposable observations.
use crate::{RequestKind, Store, Usage, id, now};
use serde::{Deserialize, Serialize};
use std::{future::Future, sync::Mutex};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RequestUsage {
    pub request_id: String,
    pub created_at: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub provider: String,
    pub account: String,
    pub model: String,
    pub kind: RequestKind,
    #[serde(flatten)]
    pub usage: Usage,
    pub failed: bool,
}

impl<'de> Deserialize<'de> for RequestUsage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            request_id: String,
            created_at: String,
            session_id: String,
            run_id: Option<String>,
            provider: String,
            account: Option<String>,
            model: String,
            kind: RequestKind,
            #[serde(flatten)]
            usage: Usage,
            failed: bool,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            request_id: wire.request_id,
            created_at: wire.created_at,
            session_id: wire.session_id,
            run_id: wire.run_id,
            account: wire.account.unwrap_or_else(|| wire.provider.clone()),
            provider: wire.provider,
            model: wire.model,
            kind: wire.kind,
            usage: wire.usage,
            failed: wire.failed,
        })
    }
}

impl RequestUsage {
    /// Create before dispatch so failures retain the same identity and timestamp.
    /// A capture scope supplies session, run and request kind at completion.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let provider = provider.into();
        let account = ACCOUNT
            .try_with(Clone::clone)
            .unwrap_or_else(|_| provider.clone());
        Self {
            request_id: id(),
            created_at: now(),
            session_id: String::new(),
            run_id: None,
            provider,
            account,
            model: model.into(),
            kind: RequestKind::Main,
            usage: Usage::default(),
            failed: false,
        }
    }
}

struct Capture {
    session_id: String,
    run_id: Option<String>,
    kind: RequestKind,
    records: Mutex<Vec<RequestUsage>>,
    store: Option<Store>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(store) = &self.store {
            let records = std::mem::take(self.records.get_mut().unwrap());
            if records.is_empty() {
                return;
            }
            let result = records
                .into_iter()
                .try_for_each(|record| store.queue_request_usage(record))
                .and_then(|()| store.flush_request_usage(&self.session_id));
            if let Err(error) = result {
                tracing::warn!("Failed to preserve interrupted request usage: {error}");
            }
        }
    }
}

tokio::task_local! {
    static CAPTURE: Capture;
    static PROVIDER: String;
    static ACCOUNT: String;
}

/// Capture physical provider attempts without writing while the network is in
/// flight. The caller commits the resulting rows with its existing state change.
/// Scopes belong to the logical operation; provider adapters only record attempts.
pub async fn capture_request_usage<F: Future>(
    session_id: &str,
    run_id: Option<&str>,
    kind: RequestKind,
    future: F,
) -> (F::Output, Vec<RequestUsage>) {
    capture(None, session_id, run_id, kind, future).await
}

/// As `capture_request_usage`, preserving completed attempts if the enclosing
/// future is dropped before its caller can commit the returned records.
pub async fn capture_request_usage_with_store<F: Future>(
    store: &Store,
    session_id: &str,
    run_id: Option<&str>,
    kind: RequestKind,
    future: F,
) -> (F::Output, Vec<RequestUsage>) {
    capture(Some(store.clone()), session_id, run_id, kind, future).await
}

async fn capture<F: Future>(
    store: Option<Store>,
    session_id: &str,
    run_id: Option<&str>,
    kind: RequestKind,
    future: F,
) -> (F::Output, Vec<RequestUsage>) {
    CAPTURE
        .scope(
            Capture {
                session_id: session_id.to_owned(),
                run_id: run_id.map(str::to_owned),
                kind,
                records: Mutex::default(),
                store,
            },
            async {
                let result = future.await;
                let records =
                    CAPTURE.with(|capture| std::mem::take(&mut *capture.records.lock().unwrap()));
                (result, records)
            },
        )
        .await
}

/// Supply the configured provider route name around an adapter invocation.
pub async fn with_request_usage_provider<F: Future>(provider: &str, future: F) -> F::Output {
    PROVIDER.scope(provider.to_owned(), future).await
}

/// Bind physical attempts to a configured account without changing provider routing.
/// Nested scopes restore their caller's account; concurrent tasks remain isolated.
pub async fn with_request_usage_account<F: Future>(account: &str, future: F) -> F::Output {
    ACCOUNT.scope(account.to_owned(), future).await
}

/// Request role of the active usage scope, or None outside a scope.
pub fn current_request_kind() -> Option<RequestKind> {
    CAPTURE.try_with(|capture| capture.kind).ok()
}

/// Number of attempts already captured, or None outside a scope. Registry
/// wrappers use this to supply a fallback only for uninstrumented adapters.
pub fn request_usage_count() -> Option<usize> {
    CAPTURE
        .try_with(|capture| capture.records.lock().unwrap().len())
        .ok()
}

/// Record a completed physical attempt. Returns false outside a capture scope.
/// Never persists prompts, response content, credentials or transport state.
pub fn record_request_usage(mut record: RequestUsage) -> bool {
    let default_account = record.account == record.provider;
    if let Ok(provider) = PROVIDER.try_with(Clone::clone) {
        record.provider = provider;
    }
    if let Ok(account) = ACCOUNT.try_with(Clone::clone) {
        record.account = account;
    } else if default_account {
        record.account.clone_from(&record.provider);
    }
    CAPTURE
        .try_with(|capture| {
            record.session_id.clone_from(&capture.session_id);
            record.run_id.clone_from(&capture.run_id);
            record.kind = capture.kind;
            capture.records.lock().unwrap().push(record);
        })
        .is_ok()
}
