//! Stderr logging for the runner binary. The core and runner report warnings
//! through `tracing`; this installs the subscriber that prints them.

use tracing_subscriber::EnvFilter;

/// Installs a stderr subscriber that shows warnings and above. `RUST_LOG`
/// (for example `RUST_LOG=debug`) replaces that filter. A subscriber that is
/// already installed is kept.
pub fn install() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let stderr = std::io::stderr();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&stderr))
        .try_init();
}
