//! Unit tests for the layered runner config.
use super::*;

mod agent_settings;
mod attach_env;
mod layering;
mod mcp_settings;
mod root_settings;
mod web_settings;
use layering::options_with_env;
mod providers_quota;
mod role_settings;
fn options(root: &Path) -> LoadOptions {
    crate::workspace_trust::trust(&root.join("home"), &root.join("workspace")).unwrap();
    LoadOptions {
        workspace: root.join("workspace"),
        home: Some(root.join("home")),
        custom_file: None,
        env: vec![],
        cli: vec![],
    }
}
fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
fn override_value(path: &str, value: &str) -> Override {
    Override::value(path, parse_value(value))
}
