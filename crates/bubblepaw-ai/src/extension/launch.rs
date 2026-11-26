//! How the provider starts its subprocess. The provider builds the command and
//! owns the protocol; the host supplies the launcher, which spawns the command
//! in a group it can kill as a whole.
use std::io;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    process::Command,
};

/// A started subprocess. Dropping it ends the process and anything it started.
pub trait LaunchedProcess: Send + Sync {
    /// End the process and everything it started, now.
    fn kill_tree(&self);
}

/// A started subprocess with its three pipes.
pub struct Launched {
    pub stdin: Box<dyn AsyncWrite + Send + Unpin>,
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    pub stderr: Box<dyn AsyncRead + Send + Unpin>,
    pub process: Box<dyn LaunchedProcess>,
}

/// Starts the command the provider built (arguments, piped stdio, kill on drop,
/// environment and working directory already set).
pub trait ExtensionLauncher: Send + Sync {
    fn launch(&self, command: &mut Command) -> io::Result<Launched>;
}
