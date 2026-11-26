//! The agent crate holds none of the session shell's vocabulary. The dependency rule
//! and the word ceilings for `bubblepaw-agent` live in `layering.rs`.
use std::fs;
use std::path::{Path, PathBuf};

/// The session shell's vocabulary, as lowercase substrings, which `bubblepaw-agent`
/// must not hold. `restore` is cut out first: it contains `store` and nothing else
/// about it is the store's. `child` and `quota` are held to zero by the agent rule's
/// word list above.
const AGENT_SHELL_WORDS: [&str; 9] = [
    "harness",
    "approval",
    "sqlite",
    "store.",
    "store::",
    "crate::store",
    "eventkind",
    "event_log",
    "rusqlite",
];

/// Lines of the agent crate that name a shell word because the model reads them:
/// changing the text would change what the model is told.
const AGENT_MODEL_VISIBLE: [(&str, &str); 1] = [(
    "discovery.rs",
    "the target keeps its normal permissions, approval and execution limits.",
)];

fn agent_sources(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let path = entry.expect("source directory entry").path();
        if path.is_dir() {
            agent_sources(&path, root, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let name = path
                .strip_prefix(root)
                .expect("source under root")
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            out.push((name, text.to_lowercase()));
        }
    }
}

#[test]
fn agent_crate_holds_no_shell_vocabulary() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bubblepaw-agent/src");
    let mut files = Vec::new();
    agent_sources(&root, &root, &mut files);
    assert!(
        files.len() > 20,
        "found too few agent sources: {}",
        files.len()
    );
    let mut found = Vec::new();
    for (file, text) in &files {
        let allowed: Vec<_> = AGENT_MODEL_VISIBLE
            .iter()
            .filter(|(path, _)| path == file)
            .map(|(_, line)| *line)
            .collect();
        for (number, line) in text.lines().enumerate() {
            if allowed.iter().any(|text| line.contains(text)) {
                continue;
            }
            let line = line.replace("restore", "");
            found.extend(
                AGENT_SHELL_WORDS
                    .iter()
                    .filter(|word| line.contains(*word))
                    .map(|word| format!("{file}:{}: {word:?}", number + 1)),
            );
        }
    }
    assert!(
        found.is_empty(),
        "bubblepaw-agent must stay free of the session shell's vocabulary; put it behind a seam:\n{}",
        found.join("\n")
    );
}
