//! Explicit PowerShell fixture programs run as native argv, never shell source.
use serde_json::{Value, json};
use std::path::Path;

pub fn powershell_file(root: &Path, name: &str, source: &str) -> Value {
    let script = root.join(name);
    // Windows PowerShell 5.1 needs a BOM for Unicode script source.
    std::fs::write(&script, format!("\u{feff}{source}")).unwrap();
    json!({
        "parts":["powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File ", ""],
        "values":[script.to_str().unwrap()]
    })
}
