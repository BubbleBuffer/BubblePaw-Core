//! Helpers for the bundled Claude Code provider extension.
//!
//! The extension lives in `extensions/claude-code` beside this crate. Cargo
//! keeps the whole repository checkout for path and git dependencies, so
//! [`bundled_dir`] resolves inside the checkout this crate was built from.

use bubblepaw_runner::provider::ExtensionConfig;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// The `extensions/claude-code` folder of the BubblePaw source this crate was
/// built from.
pub fn bundled_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions/claude-code")
}

/// An extension entry that runs the bundled Claude Code bridge with `bun`.
///
/// `id` names the instance in the provider registry; `dir` is the extension
/// folder, usually [`bundled_dir`].
pub fn extension_config(id: &str, bun: &Path, dir: &Path) -> ExtensionConfig {
    ExtensionConfig {
        id: id.to_owned(),
        account: None,
        command: bun.to_string_lossy().into_owned(),
        args: vec![
            "run".into(),
            dir.join("src/main.ts").to_string_lossy().into_owned(),
        ],
        cwd: Some(dir.to_path_buf()),
        env: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_claude_code_bundled_dir_exists() {
        let main = bundled_dir().join("src/main.ts");
        assert!(
            main.is_file(),
            "expected the bundled Claude Code extension at {}",
            main.display()
        );
    }
}
