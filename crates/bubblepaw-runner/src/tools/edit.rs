use super::{MAX_MUTATION_FILE, MAX_RESULT, WorkspaceTools, clip, string, validate_fields};
use anyhow::{Context, Result, anyhow, bail, ensure};
use bubblepaw_core::{CancellationToken, Store, ToolContext};
use serde_json::{Value, json};
use std::path::Path;

impl WorkspaceTools {
    pub(super) fn edit_or_write_checkpointed(
        &self,
        name: &str,
        input: &Value,
        context: &ToolContext,
        store: &Store,
    ) -> Result<Value> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        validate_fields(
            input,
            if name == "edit" {
                &["path", "old_string", "new_string", "replace_all", "edits"]
            } else {
                &["path", "content"]
            },
        )?;
        let path = string(input, "path")?;
        (|| -> Result<Value> {
            ensure!(
                Path::new(path).is_relative(),
                "path must be workspace-relative; use a path relative to the workspace root"
            );
            let mut change = self.prepare_file_change(path)?;
            let result = if name == "edit" {
                let previous = change
                    .previous
                    .as_deref()
                    .ok_or_else(|| anyhow!("file does not exist; use write to create it"))?;
                let source = std::str::from_utf8(previous).context("file is not UTF-8")?;
                let (content, replacements) = apply_edits(source, input, &context.cancellation)?;
                change.content = Some(content.into_bytes());
                json!({"path":change.relative,"replacements":replacements})
            } else {
                let content = string(input, "content")?;
                ensure!(
                    content.len() as u64 <= MAX_MUTATION_FILE,
                    "file exceeds 32 MiB limit"
                );
                let created = change.previous.is_none();
                change.content = Some(content.as_bytes().to_vec());
                json!({"path":change.relative,"bytes":content.len(),"created":created})
            };
            ensure!(
                serde_json::to_vec(&result)?.len() <= MAX_RESULT,
                "workspace result exceeds 60 KiB JSON limit"
            );
            self.commit_file_checkpointed(change, context, store)?;
            Ok(result)
        })()
        .map_err(|error| anyhow!("file {path:?}: {error:#}"))
    }
}

/// Applies the single-edit arguments or the `edits` array to `source`. Matching
/// ignores CRLF versus LF and a leading BOM; the write keeps both.
fn apply_edits(
    source: &str,
    input: &Value,
    cancellation: &CancellationToken,
) -> Result<(String, usize)> {
    let (bom, mut text) = match source.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest.to_owned()),
        None => ("", source.to_owned()),
    };
    let mut total = 0;
    if let Some(edits) = input.get("edits") {
        ensure!(
            ["old_string", "new_string", "replace_all"]
                .iter()
                .all(|key| input.get(key).is_none()),
            "give either edits or old_string/new_string, not both"
        );
        let edits = edits
            .as_array()
            .filter(|edits| !edits.is_empty())
            .ok_or_else(|| anyhow!("edits must be a nonempty array"))?;
        for (index, edit) in edits.iter().enumerate() {
            let step = validate_fields(edit, &["old_string", "new_string", "replace_all"])
                .and_then(|()| apply_one(&text, edit, cancellation));
            let (next, count) = step.map_err(|error| anyhow!("edits[{index}]: {error:#}"))?;
            text = next;
            total += count;
        }
    } else {
        (text, total) = apply_one(&text, input, cancellation)?;
    }
    Ok((format!("{bom}{text}"), total))
}

fn apply_one(
    source: &str,
    edit: &Value,
    cancellation: &CancellationToken,
) -> Result<(String, usize)> {
    let old = string(edit, "old_string")?;
    let new = string(edit, "new_string")?;
    ensure!(
        !old.is_empty(),
        "old_string must not be empty; use write to create a file"
    );
    ensure!(old != new, "old_string and new_string must differ");
    let replace_all = edit
        .get("replace_all")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| anyhow!("replace_all must be a boolean"))
        })
        .transpose()?
        .unwrap_or(false);
    replace_exact(source, old, new, replace_all, cancellation)
}

/// Replaces in `source` matching on an LF view of it. Untouched text keeps its
/// bytes; inserted text takes the file's majority line ending.
fn replace_exact(
    source: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    cancellation: &CancellationToken,
) -> Result<(String, usize)> {
    // `dropped` holds the LF-view index of each `\n` that had a `\r` before it.
    let mut dropped = Vec::new();
    let mut lf = String::with_capacity(source.len());
    let (mut crlf, mut bare) = (0usize, 0usize);
    let mut rest = source;
    while let Some(position) = rest.find('\n') {
        let (line, tail) = rest.split_at(position);
        if let Some(line) = line.strip_suffix('\r') {
            crlf += 1;
            lf.push_str(line);
            dropped.push(lf.len());
        } else {
            bare += 1;
            lf.push_str(line);
        }
        lf.push('\n');
        rest = &tail[1..];
    }
    lf.push_str(rest);
    let old = old.replace("\r\n", "\n");
    let new = new.replace("\r\n", "\n");
    let positions = find_matches(&lf, &old, replace_all, cancellation)?;
    let count = positions.len();
    let new = if crlf > bare {
        new.replace('\n', "\r\n")
    } else {
        new
    };
    // Both the original and final content fit the same 64 MiB combined bound
    // used by the atomic file preparation, before allocating the replacement.
    let output_bytes = source
        .len()
        .checked_sub(count * old.len())
        .and_then(|remaining| new.len().checked_mul(count)?.checked_add(remaining))
        .ok_or_else(|| anyhow!("replacement exceeds 32 MiB file limit"))?;
    ensure!(
        output_bytes as u64 <= MAX_MUTATION_FILE,
        "replacement exceeds 32 MiB file limit"
    );
    ensure!(!cancellation.is_cancelled(), "cancelled");
    // A LF-view index maps to the source by adding the CRs dropped before it.
    let original = |index: usize| index + dropped.partition_point(|&line| line < index);
    let mut output = String::with_capacity(output_bytes);
    let mut cursor = 0;
    for position in positions {
        let (start, end) = (original(position), original(position + old.len()));
        output.push_str(&source[cursor..start]);
        output.push_str(&new);
        cursor = end;
    }
    output.push_str(&source[cursor..]);
    Ok((output, count))
}

/// Byte positions in the LF view; exactly one unless `replace_all`.
fn find_matches(
    source: &str,
    old: &str,
    replace_all: bool,
    cancellation: &CancellationToken,
) -> Result<Vec<usize>> {
    let mut positions = Vec::new();
    let mut count = 0usize;
    let mut first_lines = Vec::new();
    let mut line = 1usize;
    let mut cursor = 0usize;
    for (position, _) in source.match_indices(old) {
        ensure!(!cancellation.is_cancelled(), "cancelled");
        count += 1;
        positions.push(position);
        if first_lines.len() < 5 {
            line += source.as_bytes()[cursor..position]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count();
            first_lines.push(line);
            cursor = position;
        }
    }
    if count == 0 {
        bail!(
            "old_string has no exact match. {} Copy old_string exactly from a fresh read, including indentation and line endings.",
            nearest_region(source, old, cancellation)?
        );
    }
    ensure!(
        count == 1 || replace_all,
        "old_string matches {count} occurrences; first matching line numbers: {}. Add surrounding context to make old_string unique, or set replace_all to true.",
        first_lines
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(positions)
}

// Diagnostics never influence replacement. A cheap linear scan of the whole
// bounded file keeps a small shortlist; detailed distance and output stay bounded.
fn nearest_region(source: &str, old: &str, cancellation: &CancellationToken) -> Result<String> {
    const COMPARE_CHARS: usize = 128;
    const CANDIDATES: usize = 16;
    let expected = old.split('\n').next().unwrap_or("");
    let expected_chars: Vec<_> = expected.chars().take(COMPARE_CHARS).collect();
    let expected_fingerprint = fingerprint(&expected_chars);
    let expected_bits: u32 = expected_fingerprint
        .iter()
        .map(|part| part.count_ones())
        .sum();
    // (line number, byte offset, line, similarity numerator, denominator)
    let mut candidates: Vec<(usize, usize, &str, u32, u32)> = Vec::new();
    let mut offset = 0;
    for (index, line) in source.split('\n').enumerate() {
        ensure!(!cancellation.is_cancelled(), "cancelled");
        let characters: Vec<_> = line.chars().take(COMPARE_CHARS).collect();
        let signature = fingerprint(&characters);
        let bits: u32 = signature.iter().map(|part| part.count_ones()).sum();
        let overlap: u32 = signature
            .iter()
            .zip(&expected_fingerprint)
            .map(|(left, right)| (left & right).count_ones())
            .sum();
        let (score, width) = if bits + expected_bits == 0 {
            (1, 1)
        } else {
            (2 * overlap, bits + expected_bits)
        };
        let position = candidates
            .iter()
            .position(|candidate| score * candidate.4 > candidate.3 * width)
            .unwrap_or(candidates.len());
        if position < CANDIDATES {
            candidates.insert(position, (index, offset, line, score, width));
            candidates.truncate(CANDIDATES);
        }
        offset += line.len() + 1;
    }
    let mut best = candidates[0];
    let mut best_distance = usize::MAX;
    let mut best_width = 1usize;
    for candidate in candidates {
        let characters: Vec<_> = candidate.2.chars().take(COMPARE_CHARS).collect();
        let width = characters.len().max(expected_chars.len()).max(1);
        let distance = edit_distance(&expected_chars, &characters);
        if best_distance == usize::MAX || distance * best_width < best_distance * width {
            best = candidate;
            best_distance = distance;
            best_width = width;
        }
    }
    let start = if best.1 == 0 {
        0
    } else {
        source[..best.1 - 1]
            .rfind('\n')
            .map_or(0, |position| position + 1)
    };
    let start_line = best.0.saturating_sub(1);
    let mut message = String::from("Nearest region (similarity diagnostic):\n");
    for (index, line) in source[start..].split('\n').take(3).enumerate() {
        let (text, clipped) = clip(line.trim_end_matches('\r'), 240);
        message.push_str(&format!(
            "{}: {}{}\n",
            start_line + index + 1,
            text,
            if clipped { "…" } else { "" }
        ));
    }
    let actual: Vec<_> = best.2.chars().take(COMPARE_CHARS).collect();
    let difference = expected_chars
        .iter()
        .zip(&actual)
        .position(|(left, right)| left != right)
        .or_else(|| {
            (expected_chars.len() != actual.len()).then_some(expected_chars.len().min(actual.len()))
        });
    if let Some(index) = difference {
        let describe = |character: Option<&char>| {
            character.map_or_else(
                || "end of line".to_owned(),
                |character| format!("{character:?}"),
            )
        };
        message.push_str(&format!(
            "First difference on line {}, character {}: old_string has {}, file has {}.\n",
            best.0 + 1,
            index + 1,
            describe(expected_chars.get(index)),
            describe(actual.get(index))
        ));
    }
    Ok(message)
}

// Character bigrams provide a cheap order-sensitive resemblance score. Hash
// collisions affect only the diagnostic shortlist, never exact matching.
fn fingerprint(characters: &[char]) -> [u64; 4] {
    let mut signature = [0u64; 4];
    let mut previous = 0u32;
    for &character in characters {
        let hash = previous.wrapping_mul(31) ^ character as u32;
        let bit = (hash & 255) as usize;
        signature[bit / 64] |= 1u64 << (bit % 64);
        previous = character as u32;
    }
    signature
}

fn edit_distance(left: &[char], right: &[char]) -> usize {
    let mut previous: Vec<_> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (row, left) in left.iter().enumerate() {
        current[0] = row + 1;
        for (column, right) in right.iter().enumerate() {
            current[column + 1] = (current[column] + 1)
                .min(previous[column + 1] + 1)
                .min(previous[column] + usize::from(left != right));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}
