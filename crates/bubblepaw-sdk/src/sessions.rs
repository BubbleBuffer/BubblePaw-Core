//! Typed title search through the host's ordinary session listing API.
use crate::{Client, ClientError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

const TITLE_SEARCH_FEATURE: &str = "session.list.title_search";

fn ensure_title_search(initialized: &serde_json::Value) -> Result<(), ClientError> {
    if initialized["features"].as_array().is_some_and(|features| {
        features
            .iter()
            .any(|feature| feature == TITLE_SEARCH_FEATURE)
    }) {
        return Ok(());
    }
    Err(ClientError::Protocol(format!(
        "host does not support {TITLE_SEARCH_FEATURE}; upgrade the host to use title search"
    )))
}

/// A title-search page, with the same rows as `session.list`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionWithUsage>,
    pub next_cursor: Option<String>,
}

/// Session metadata and the host's list projections. These wire types remain
/// available to remote clients built without the SDK's `host` feature.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionWithUsage {
    #[serde(flatten)]
    pub session: bubblepaw_core::Session,
    pub usage: serde_json::Value,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    #[serde(default)]
    pub needs_you: bool,
    #[serde(default)]
    pub waiting_for: Option<WaitingFor>,
    #[serde(default)]
    pub running: bool,
}

/// The reason a session needs a person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitingFor {
    Approval,
    Question,
}

impl Client {
    /// Find saved chat titles across the host's readable projects. The query is
    /// a literal case-insensitive substring (Unicode lowercase, not regex).
    /// Hidden sessions are excluded; archives retain ordinary list behavior.
    ///
    /// `limit` must be 1..=1000. Pass the returned cursor and the same query for
    /// subsequent pages. Pages sort newest creation first, then ID descending;
    /// an empty query browses all readable titles. A workspace-scoped host still
    /// restricts its results to that workspace.
    ///
    /// Each call first reads `initialize` capabilities and returns
    /// [`ClientError::Protocol`] when the host lacks `session.list.title_search`.
    /// Older hosts otherwise silently ignore the query and return unrelated rows.
    pub async fn search_session_titles(
        &self,
        query: impl Into<String>,
        limit: u64,
        cursor: Option<String>,
    ) -> Result<SessionPage, ClientError> {
        ensure_title_search(&self.call("initialize", json!({})).await?)?;
        let page = self
            .call(
                "session.list",
                json!({"query": query.into(), "limit": limit, "cursor": cursor}),
            )
            .await?;
        serde_json::from_value(page)
            .map_err(|error| ClientError::Protocol(format!("invalid session list page: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_search_requires_explicit_host_support() {
        for initialized in [
            json!({}),
            json!({"features":null}),
            json!({"features":["session.list", "session.list.pagination"]}),
        ] {
            let error = ensure_title_search(&initialized).unwrap_err();
            assert!(matches!(error, ClientError::Protocol(_)));
            assert!(error.to_string().contains("session.list.title_search"));
        }
        ensure_title_search(&json!({"features":["session.list.title_search"]})).unwrap();
    }
}
