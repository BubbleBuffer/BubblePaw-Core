use std::fmt;

/// One application error shared by JSON-RPC, CLI and HTTP adapters.
#[derive(Debug)]
pub struct HostError {
    pub message: String,
    pub status: u16,
    pub code: i32,
    source: Option<anyhow::Error>,
}
impl HostError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status,
            code: -32000,
            source: None,
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(503, message)
    }
    pub fn denied(method: &str) -> Self {
        Self {
            message: format!("Host profile does not permit {method}"),
            status: 403,
            code: -32003,
            source: None,
        }
    }
    /// The main model route has no usable provider: `-32004` and HTTP 409, with
    /// the message naming the config keys and host methods that fix it.
    pub fn no_provider(error: crate::no_provider::NoProviderConfigured) -> Self {
        Self {
            message: error.to_string(),
            status: 409,
            code: crate::no_provider::CODE,
            source: Some(error.into()),
        }
    }
    pub fn unknown(method: &str) -> Self {
        Self::new(400, format!("unknown method: {method}"))
    }
    pub fn rpc_code(&self) -> i32 {
        self.code
    }
    pub fn http_status(&self) -> u16 {
        self.status
    }
    pub fn into_inner(self) -> anyhow::Error {
        self.source.unwrap_or_else(|| anyhow::anyhow!(self.message))
    }
}
impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for HostError {}
impl From<anyhow::Error> for HostError {
    fn from(error: anyhow::Error) -> Self {
        let error = match error.downcast::<Self>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        use crate::{context_reads::InvalidContextRead, session_control::WorkspaceMismatch};
        if let Some(missing) = error.downcast_ref::<crate::no_provider::NoProviderConfigured>() {
            return Self::no_provider(missing.clone());
        }
        if let Some(set) = error.downcast_ref::<crate::config_set::ConfigSetError>() {
            return Self {
                message: set.to_string(),
                status: set.status(),
                code: set.code(),
                source: None,
            };
        }
        let text = error.to_string();
        let text = if text.contains("database is locked") {
            let causes: Vec<String> = error.chain().map(|c| format!("{c:?}")).collect();
            let bt = error.backtrace().to_string();
            let frames: Vec<&str> = bt
                .lines()
                .map(str::trim)
                .filter(|l| l.contains("bubblepaw_") || l.contains("rusqlite"))
                .take(14)
                .collect();
            format!("{text} DIAGSQL causes={causes:?} frames={frames:?}")
        } else {
            text
        };
        let status = if let Some(error) =
            error.downcast_ref::<crate::question::ChildQuestionAnswerError>()
        {
            use crate::question::ChildQuestionAnswerError::*;
            match error {
                NotFound => 404,
                Conflict => 409,
                Invalid => 400,
                Internal(_) => 500,
            }
        } else if let Some(error) = error.downcast_ref::<crate::provider_setup::ApiError>() {
            error.0.as_u16()
        } else if error.downcast_ref::<WorkspaceMismatch>().is_some()
            || error
                .downcast_ref::<crate::session_scope::SessionLeaseBusy>()
                .is_some()
        {
            409
        } else if error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|e| matches!(e, rusqlite::Error::QueryReturnedNoRows))
        {
            404
        } else if error.downcast_ref::<InvalidContextRead>().is_some() {
            400
        } else if text.contains("active")
            || text.contains("unresolved tool")
            || text.contains("shutting down")
        {
            409
        } else if text.contains("unknown or expired") || text.contains("another conversation") {
            404
        } else if text.contains("size limit")
            || text.contains("read only")
            || text.contains("unsupported handle")
            || text.contains("not available in this host")
        {
            400
        } else {
            500
        };
        Self {
            message: text,
            status,
            code: -32000,
            source: Some(error),
        }
    }
}
