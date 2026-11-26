//! The one-run loop stays free of the session shell's vocabulary (see `layering.rs`).
use std::fs;
use std::path::PathBuf;

/// The loop holds seams and plain values: none of the harness's own vocabulary.
/// The dispatcher it drives knows no approval type either.
#[test]
fn run_loop_files_hold_no_harness_vocabulary() {
    const LOOP_WORDS: [&str; 6] = ["harness", "approval", "sqlite", "store.", "quota", "child"];
    const FILES: [(&str, &[&str]); 4] = [
        ("run_loop.rs", &LOOP_WORDS),
        ("run_settle.rs", &LOOP_WORDS),
        ("dispatch.rs", &["approval"]),
        ("dispatch/calls.rs", &["approval"]),
    ];
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for (file, words) in FILES {
        let source = fs::read_to_string(src.join(file))
            .unwrap_or_else(|error| panic!("read {file}: {error}"))
            .to_lowercase();
        found.extend(
            words
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
