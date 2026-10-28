//! Optional chapter v2 retrieval blocks. Invalid metadata never invalidates prose.
use bubblepaw_core::ChapterSection;

#[derive(Debug)]
pub(super) struct Metadata {
    pub decisions: Vec<String>,
    pub sections: Vec<ChapterSection>,
}

/// Split at a block marker even if its payload is malformed, so fallback only
/// retains ordinary chapter memory. V1 never calls this function.
pub(super) fn prose(text: &str) -> &str {
    let offset = text
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line.trim()))
        })
        .find(|(_, line)| line.starts_with("Decisions:") || line.starts_with("Sections:"))
        .map_or(text.len(), |(offset, _)| offset);
    text[..offset].trim_end()
}

pub(super) fn parse(text: &str, episodes: &[String]) -> Result<Metadata, &'static str> {
    let body = prose(text);
    let suffix = &text[body.len()..];
    let mut lines = suffix
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let mut decisions = Vec::new();
    match lines.next() {
        Some("Decisions: none") => {
            if lines.next() != Some("Sections:") {
                return Err("missing_sections");
            }
        }
        Some("Decisions:") => {
            loop {
                match lines.next() {
                    Some("Sections:") => break,
                    Some(line) => {
                        let decision = line.strip_prefix("- ").ok_or("invalid_decision")?;
                        if decision.trim().is_empty() || decision.chars().count() > 100 {
                            return Err("invalid_decision");
                        }
                        decisions.push(decision.to_owned());
                    }
                    None => return Err("missing_sections"),
                }
            }
            if decisions.is_empty() {
                return Err("empty_decisions");
            }
        }
        _ => return Err("missing_decisions"),
    }
    let ordinals = shown_ordinals(episodes)?;
    let mut covered = 0;
    let mut sections = Vec::new();
    for line in lines {
        let row = line.strip_prefix("- ").ok_or("invalid_section")?;
        let (name, range) = row.split_once(" | episodes ").ok_or("invalid_section")?;
        let words = name.split_whitespace().count();
        if !(1..=5).contains(&words) || name.contains('|') || name != name.trim() {
            return Err("invalid_section_name");
        }
        let (first, last) = range.split_once('-').ok_or("invalid_section_range")?;
        let first = ordinal(first).ok_or("invalid_section_range")?;
        let last = ordinal(last).ok_or("invalid_section_range")?;
        if first > last || ordinals.get(covered) != Some(&first) {
            return Err("section_coverage");
        }
        // Consume only actual input ordinals, without expanding untrusted ranges.
        while covered < ordinals.len() && ordinals[covered] <= last {
            covered += 1;
        }
        if ordinals.get(covered.saturating_sub(1)) != Some(&last) {
            return Err("section_coverage");
        }
        sections.push(ChapterSection {
            name: name.to_owned(),
            first_episode: first,
            last_episode: last,
        });
    }
    if sections.is_empty() || covered != ordinals.len() {
        return Err("section_coverage");
    }
    Ok(Metadata {
        decisions,
        sections,
    })
}

fn ordinal(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|ordinal| *ordinal > 0)
}

fn shown_ordinals(episodes: &[String]) -> Result<Vec<usize>, &'static str> {
    let ordinals: Vec<_> = episodes
        .iter()
        .map(|episode| {
            episode
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("EPISODE "))
                .and_then(|line| line.split_once(" [source "))
                .and_then(|(number, _)| ordinal(number))
                .ok_or("invalid_input_ordinals")
        })
        .collect::<Result<_, _>>()?;
    if ordinals.is_empty()
        || ordinals
            .windows(2)
            .any(|pair| pair[0].checked_add(1) != Some(pair[1]))
    {
        return Err("invalid_input_ordinals");
    }
    Ok(ordinals)
}

#[cfg(test)]
#[path = "chapter_sections_tests.rs"]
mod tests;
