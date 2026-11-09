//! The options, backend and status types of the remote server.
use super::Limits;
use super::reach::RemoteReach;
use super::serve_lock::ProcessKind;
use crate::host_api::HostCtx;
use std::path::{Path, PathBuf};

/// Where tunnels are served.
#[derive(Clone)]
pub enum SessionBackend {
    /// In this process, on the host's shared context.
    Context(Box<HostCtx>),
    /// Through the host that owns this data directory, over its control
    /// endpoint (`attach.session` with the device's profile).
    Attach(PathBuf),
}

#[derive(Clone, Debug)]
pub struct RemoteServeOptions {
    /// How devices reach the host.
    pub reach: RemoteReach,
    /// The host device key, created on first use.
    pub key_file: PathBuf,
    /// The paired-device store.
    pub clients: PathBuf,
    /// The name devices see while pairing.
    pub host_name: String,
    /// Relay limits (relay reach only).
    pub limits: Limits,
    /// The serve lock to hold while serving, so a second server for the same
    /// data directory (this CLI or another embedder) refuses to start. `None`
    /// serves without it, for tests that share one directory.
    pub lock: Option<PathBuf>,
    /// Recorded with the lock so [`super::serve_status`] can say who shares.
    pub process: ProcessKind,
}

impl RemoteServeOptions {
    /// Options with the standard paths in `data_dir` and default limits.
    pub fn in_data_dir(data_dir: &Path, reach: RemoteReach, host_name: String) -> Self {
        let paths = super::RemotePaths::in_data_dir(data_dir);
        Self {
            reach,
            key_file: paths.key_file,
            clients: paths.clients,
            host_name,
            limits: Limits::default(),
            lock: Some(paths.lock),
            process: ProcessKind::Embedded,
        }
    }
}
