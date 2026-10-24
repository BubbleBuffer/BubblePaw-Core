use super::*;

impl McpHost {
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.inner
            .state
            .lock()
            .unwrap()
            .tools
            .values()
            .map(|t| {
                Arc::new(McpTool {
                    host: Arc::downgrade(&self.inner),
                    definition: t.definition.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }
    /// Publish one inventory snapshot with its discovery policy and original
    /// server identity, including names shortened in the registered tool name.
    pub fn register_tools(&self, registry: &mut ToolRegistry) -> Result<()> {
        let state = self.inner.state.lock().unwrap();
        for registered in state.tools.values() {
            let name = registered.definition.name.clone();
            registry.register_host(Arc::new(McpTool {
                host: Arc::downgrade(&self.inner),
                definition: registered.definition.clone(),
            }))?;
            registry.set_deferred(name, state.servers[&registered.server_name].config.defer);
            registry.set_tool_server(&registered.definition.name, &registered.server_name);
        }
        Ok(())
    }
}
