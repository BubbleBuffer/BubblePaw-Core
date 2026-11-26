//! How registered host capabilities are presented to a model.
use serde::{Deserialize, Serialize};

/// How registered host capabilities are presented to a model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Offer only the registered `js` composition adapter to the model.
    Js,
    /// Offer registered native capabilities directly, without `js`.
    #[default]
    Native,
}
