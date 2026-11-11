//! Output normalization for comparing an attached run with a standalone one.
use regex::Regex;
use std::path::Path;

pub(crate) fn normalize(text: &str, data: &Path) -> String {
    let uuid = Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let stamp =
        Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})").unwrap();
    let timing = Regex::new(
        r#""([A-Za-z_]*(?:_ms|_at|_secs|_nanos|_micros|_time|_duration)|pid|sequence)":\s*[0-9.]+"#,
    )
    .unwrap();
    // Extension stderr is logged at warn; whether the fake extension's startup
    // banner is logged before the run ends depends on timing.
    let text: String = text
        .lines()
        .filter(|line| !line.contains("bubblepaw::extension: extension stderr: "))
        .flat_map(|line| [line, "\n"])
        .collect();
    let text = text.replace(data.to_str().unwrap(), "<data>");
    let text = uuid.replace_all(&text, "<uuid>");
    let text = stamp.replace_all(&text, "<time>");
    timing.replace_all(&text, r#""$1":0"#).into_owned()
}
