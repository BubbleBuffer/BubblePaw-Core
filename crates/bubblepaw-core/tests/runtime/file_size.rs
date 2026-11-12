//! File-size ratchet: hand-written Rust and TypeScript/JavaScript source stays at
//! 500 lines or fewer. Files already over the limit are listed in
//! `file-size-allowlist.txt` at the repo root with their line count as a ceiling;
//! the ceilings only move down.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const LIMIT: usize = 500;
const ALLOWLIST: &str = "file-size-allowlist.txt";
const RUST_EXTENSIONS: &[&str] = &["rs"];
const SCRIPT_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];
const SKIPPED_DIRS: &[&str] = &["target", "node_modules", "dist", "build"];

#[test]
fn source_files_respect_the_line_limit() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let counts = scan(&root);
    assert!(
        counts.contains_key("crates/bubblepaw-core/src/lib.rs")
            && counts.contains_key("extensions/claude-code/src/main.ts"),
        "scan found too few source files under {}",
        root.display()
    );
    let allowlist = read_allowlist(&root);
    let mut failures = Vec::new();
    for (file, &lines) in &counts {
        match allowlist.get(file) {
            None if lines > LIMIT => failures.push(format!(
                "{file}: {lines} lines, over the {LIMIT}-line limit. Split it into smaller modules; new files cannot be allowlisted."
            )),
            None => {}
            Some(_) if lines <= LIMIT => failures.push(format!(
                "{file}: now {lines} lines, within the {LIMIT}-line limit. Remove its line from {ALLOWLIST}."
            )),
            Some(&ceiling) if lines > ceiling => failures.push(format!(
                "{file}: {lines} lines, over its ceiling of {ceiling} in {ALLOWLIST}. Split the file; do not raise the ceiling."
            )),
            Some(&ceiling) if lines < ceiling => failures.push(format!(
                "{file}: {lines} lines, below its ceiling of {ceiling}. Lower its number in {ALLOWLIST} to {lines}."
            )),
            Some(_) => {}
        }
    }
    for file in allowlist.keys().filter(|file| !counts.contains_key(*file)) {
        failures.push(format!(
            "{file}: listed in {ALLOWLIST} but not a scanned source file (missing, renamed or excluded). Remove its line or fix the path."
        ));
    }
    assert!(
        failures.is_empty(),
        "file size ratchet failed:\n{}",
        failures.join("\n")
    );
}

/// Line counts of every counted source file, keyed by repo-relative path with `/` separators.
fn scan(root: &Path) -> BTreeMap<String, usize> {
    let mut files = Vec::new();
    collect(&root.join("crates"), RUST_EXTENSIONS, &mut files);
    collect(&root.join("extensions"), SCRIPT_EXTENSIONS, &mut files);
    let mut counts = BTreeMap::new();
    for path in files {
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if is_generated(&bytes) {
            continue;
        }
        let relative: Vec<_> = path
            .strip_prefix(root)
            .expect("file under repo root")
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        counts.insert(relative.join("/"), line_count(&bytes));
    }
    counts
}

fn collect(dir: &Path, extensions: &[&str], out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        let name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            if !name.starts_with('.') && !SKIPPED_DIRS.contains(&name.as_str()) {
                collect(&path, extensions, out);
            }
        } else if !name.ends_with(".d.ts")
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| extensions.contains(&ext))
        {
            out.push(path);
        }
    }
}

/// Same count as `wc -l`, plus one for a final line without a newline.
fn line_count(bytes: &[u8]) -> usize {
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count();
    newlines + usize::from(bytes.last().is_some_and(|&byte| byte != b'\n'))
}

/// Generated files are not hand-written, so they are not counted.
fn is_generated(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    head.lines()
        .take(5)
        .any(|line| line.contains("@generated") || line.contains("DO NOT EDIT"))
}

/// Parses `<path> <max_lines>` entries; blank lines and `#` comments are skipped.
fn read_allowlist(root: &Path) -> BTreeMap<String, usize> {
    let text = fs::read_to_string(root.join(ALLOWLIST))
        .unwrap_or_else(|e| panic!("read {ALLOWLIST}: {e}"));
    let mut entries = BTreeMap::new();
    let mut previous = String::new();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (file, ceiling) = match fields.as_slice() {
            [file, ceiling] => (*file, *ceiling),
            _ => panic!("{ALLOWLIST}:{line_no}: expected `<path> <max_lines>`, found `{line}`"),
        };
        let ceiling: usize = ceiling
            .parse()
            .unwrap_or_else(|_| panic!("{ALLOWLIST}:{line_no}: `{ceiling}` is not a line count"));
        assert!(
            file > previous.as_str(),
            "{ALLOWLIST}:{line_no}: `{file}` must come after `{previous}`; keep the list sorted and unique"
        );
        previous = file.to_string();
        entries.insert(file.to_string(), ceiling);
    }
    entries
}
