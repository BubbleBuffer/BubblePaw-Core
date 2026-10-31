use super::*;

pub(super) struct Entry {
    pub(super) server: Arc<Server>,
    pub(super) config: McpServerSettings,
    pub(super) listed: Vec<Value>,
}
#[derive(Clone)]
pub(super) struct Registered {
    pub(super) server_name: String,
    pub(super) server: Arc<Server>,
    pub(super) original_name: String,
    pub(super) definition: ToolDefinition,
}
