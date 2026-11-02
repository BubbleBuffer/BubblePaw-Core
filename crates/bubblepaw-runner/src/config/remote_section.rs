//! `[remote]`: relay access for `bubblepaw remote serve` and `remote pair`.
use super::*;

config_section! {
    /// Remote access through a relay (`bubblepaw remote ...`). Read at startup.
    #[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
    #[serde(default)]
    pub struct RemoteSettings {
        /// The relay's WebSocket URL (`wss://...`; `ws://` for local testing).
        pub relay: Option<String> => RESTART,
        /// `bubblepaw remote serve` refuses to start unless this is true.
        pub enabled: bool => RESTART,
    }
}
impl RemoteSettings {
    pub(super) fn validate(&self) -> Result<()> {
        if let Some(relay) = &self.relay {
            ensure!(
                relay.starts_with("wss://") || relay.starts_with("ws://"),
                "remote.relay must be a ws:// or wss:// URL"
            );
        }
        Ok(())
    }
}
