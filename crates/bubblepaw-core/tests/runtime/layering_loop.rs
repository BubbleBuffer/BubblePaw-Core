//! The one-run loop stays free of the session shell's vocabulary (see `layering.rs`).
use std::fs;
use std::path::PathBuf;

/// The loop holds seams and plain values: none of the harness's own vocabulary.
#[test]
fn run_loop_files_hold_no_harness_vocabulary() {
    const FILES: [&str; 2] = ["run_loop.rs", "run_settle.rs"];
    const WORDS: [&str; 6] = ["harness", "approval", "sqlite", "store.", "quota", "child"];
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for file in FILES {
        let source = fs::read_to_string(src.join(file))
            .unwrap_or_else(|error| panic!("read {file}: {error}"))
            .to_lowercase();
        found.extend(
            WORDS
                .iter()
                .filter(|word| source.contains(*word))
                .map(|word| format!("{file}: {word:?}")),
        );
    }
    assert!(
        found.is_empty(),
        "the run loop must stay free of the session shell's vocabulary; put it behind a seam:\n{}",
        found.join("\n")
    );
}
