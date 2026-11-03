//! The host-API profile a device or session holds.
use serde::{Deserialize, Serialize};

/// Profiles are cumulative. Full additionally permits host configuration,
/// process and environment controls, session deletion and shutdown.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Read,
    Edit,
    Full,
}
