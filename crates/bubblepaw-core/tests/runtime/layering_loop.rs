//! The moving set stays self-contained: the files that make up one run in memory
//! (the loop, the dispatcher, the tools and the plain data between them) hold none
//! of the session shell's vocabulary and import nothing from `crate` that is not
//! itself in the set, so they can move to a crate of their own (see `layering.rs`).
use std::fs;
use std::path::PathBuf;

/// Every file of the moving set, relative to `src`.
const MOVING: [&str; 32] = [
    "dispatch.rs",
    "dispatch/calls.rs",
    "discovery.rs",
    "execution_mode.rs",
    "extensions.rs",
    "ids.rs",
    "live_events.rs",
    "loop_hooks.rs",
    "loop_options.rs",
    "loop_records.rs",
    "observer.rs",
    "prepare_types.rs",
    "reminder_types.rs",
    "request_seam.rs",
    "round_reminders.rs",
    "run_data.rs",
    "run_inputs.rs",
    "run_journal.rs",
    "run_loop.rs",
    "run_settle.rs",
    "runtime_overflow.rs",
    "runtime_stream.rs",
    "script.rs",
    "stream.rs",
    "submit_result.rs",
    "tool.rs",
    "tool_gate.rs",
    "tool_notices.rs",
    "tool_registry.rs",
    "tool_result.rs",
    "tool_source.rs",
    "tool_views.rs",
];

/// The shell's vocabulary, as lowercase substrings. `restore` is cut out first: it
/// contains `store` and nothing else about it is the store's.
const WORDS: [&str; 11] = [
    "harness",
    "approval",
    "sqlite",
    "quota",
    "child",
    "store.",
    "store::",
    "crate::store",
    "eventkind",
    "event_log",
    "rusqlite",
];

/// Lines that name a word because the model reads them: changing the text would
/// change what the model is told.
const MODEL_VISIBLE: [(&str, &str); 1] = [(
    "discovery.rs",
    "the target keeps its normal permissions, approval and execution limits.",
)];

fn source(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {file}: {error}"))
}

/// The top-level module a moving file belongs to.
fn module(file: &str) -> &str {
    let top = file.split('/').next().expect("file name");
    top.strip_suffix(".rs").unwrap_or(top)
}

#[test]
fn moving_files_hold_no_shell_vocabulary() {
    let mut found = Vec::new();
    for file in MOVING {
        let allowed: Vec<_> = MODEL_VISIBLE
            .iter()
            .filter(|(path, _)| *path == file)
            .map(|(_, line)| *line)
            .collect();
        for (number, line) in source(file).to_lowercase().lines().enumerate() {
            if allowed.iter().any(|text| line.contains(text)) {
                continue;
            }
            let line = line.replace("restore", "");
            found.extend(
                WORDS
                    .iter()
                    .filter(|word| line.contains(*word))
                    .map(|word| format!("{file}:{}: {word:?}", number + 1)),
            );
        }
    }
    assert!(
        found.is_empty(),
        "the moving set must stay free of the session shell's vocabulary; put it behind a seam:\n{}",
        found.join("\n")
    );
}

#[test]
fn moving_files_import_only_moving_files() {
    let modules: Vec<_> = MOVING.iter().map(|file| module(file)).collect();
    let mut found = Vec::new();
    for file in MOVING {
        let text = source(file);
        if text.contains("::*") {
            found.push(format!("{file}: glob import; name what it uses"));
        }
        for (at, _) in text.match_indices("crate::") {
            let name: String = text[at + "crate::".len()..]
                .chars()
                .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                .collect();
            if name.is_empty() {
                found.push(format!(
                    "{file}: `crate::` followed by a group; import one module per line"
                ));
            } else if !modules.contains(&name.as_str()) {
                found.push(format!("{file}: crate::{name} is not in the moving set"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the moving set may import only from itself and from bubblepaw_ai:\n{}",
        found.join("\n")
    );
}
