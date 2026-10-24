//! Deterministic structural passages and local BM25 retrieval, without model calls.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

const MAX_WORDS: usize = 300;
// Also bound very long tokens and scripts without whitespace.
const MAX_CHARS: usize = 2400;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Passage {
    pub number: usize,
    pub start_offset: usize,
    pub end_offset: usize,
    pub text: String,
}

/// Blank lines and headings are preferred boundaries. Oversized blocks split at
/// whitespace, or at a Unicode character boundary if no whitespace is available.
pub(super) fn split(text: &str) -> Vec<Passage> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut start_char = 0;
    let mut words = 0;
    let mut chars = 0;
    let mut was_space = true;
    let mut previous_newline = false;
    for (byte, ch) in text.char_indices() {
        let is_space = ch.is_whitespace();
        let heading = ch == '#' && (byte == 0 || text.as_bytes()[byte - 1] == b'\n');
        let paragraph = ch == '\n' && previous_newline;
        if byte > start
            && (heading
                || (paragraph && words >= 120)
                || ((words >= MAX_WORDS || chars >= MAX_CHARS) && (is_space || chars >= MAX_CHARS)))
        {
            ranges.push((start, byte, start_char, start_char + chars));
            start = byte;
            start_char += chars;
            words = 0;
            chars = 0;
            was_space = true;
        }
        if !is_space && was_space {
            words += 1;
        }
        chars += 1;
        was_space = is_space;
        previous_newline = ch == '\n';
    }
    if start < text.len() {
        ranges.push((start, text.len(), start_char, start_char + chars));
    }
    let mut passages: Vec<Passage> = Vec::new();
    let mut leading = None;
    for (mut start, end, mut start_offset, end_offset) in ranges {
        if text[start..end].trim().is_empty() {
            if let Some(previous) = passages.last_mut() {
                previous.text.push_str(&text[start..end]);
                previous.end_offset = end_offset;
            } else {
                leading.get_or_insert((start, start_offset));
            }
            continue;
        }
        if let Some((byte, offset)) = leading.take() {
            start = byte;
            start_offset = offset;
        }
        passages.push(Passage {
            number: passages.len() + 1,
            start_offset,
            end_offset,
            text: text[start..end].to_owned(),
        });
    }
    passages
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Standard BM25 (k1=1.2, b=0.75); ties retain document order. No-match queries
/// return no passages, rather than pretending arbitrary page text is relevant.
pub(super) fn rank(passages: &[Passage], query: &str, top_k: usize) -> Vec<Passage> {
    let query: BTreeSet<_> = tokens(query).into_iter().collect();
    if query.is_empty() || passages.is_empty() {
        return Vec::new();
    }
    let documents: Vec<_> = passages.iter().map(|p| tokens(&p.text)).collect();
    let average = documents.iter().map(Vec::len).sum::<usize>() as f64 / documents.len() as f64;
    let mut frequencies: HashMap<&str, usize> = HashMap::new();
    for document in &documents {
        for term in document.iter().map(String::as_str).collect::<HashSet<_>>() {
            *frequencies.entry(term).or_default() += 1;
        }
    }
    let mut scores: Vec<_> = documents
        .iter()
        .enumerate()
        .filter_map(|(index, document)| {
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for term in document {
                *counts.entry(term).or_default() += 1;
            }
            let mut score = 0.0;
            for term in &query {
                let tf = *counts.get(term.as_str()).unwrap_or(&0) as f64;
                if tf == 0.0 {
                    continue;
                }
                let df = *frequencies.get(term.as_str()).unwrap_or(&0) as f64;
                let idf = (1.0 + (documents.len() as f64 - df + 0.5) / (df + 0.5)).ln();
                score += idf * tf * 2.2
                    / (tf + 1.2 * (0.25 + 0.75 * document.len() as f64 / average.max(1.0)));
            }
            (score > 0.0).then_some((index, score))
        })
        .collect();
    scores.sort_by(|(a, left), (b, right)| right.total_cmp(left).then_with(|| a.cmp(b)));
    scores
        .into_iter()
        .take(top_k)
        .map(|(index, _)| passages[index].clone())
        .collect()
}

pub(super) fn overlapping(passages: &[Passage], start: usize, end: usize) -> Vec<Passage> {
    passages
        .iter()
        .filter(|p| p.start_offset < end && p.end_offset > start)
        .map(|p| {
            let from = start.max(p.start_offset);
            let to = end.min(p.end_offset);
            Passage {
                number: p.number,
                start_offset: from,
                end_offset: to,
                text: p
                    .text
                    .chars()
                    .skip(from - p.start_offset)
                    .take(to - from)
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_numbers_and_bm25_find_passage_seven() {
        let text: String = (1..=10)
            .map(|number| {
                format!(
                    "# Section {number}\n\n{}\n\n",
                    if number == 7 {
                        "Unique platypus aqueduct evidence. ".repeat(40)
                    } else {
                        "Ordinary background explanation. ".repeat(40)
                    }
                )
            })
            .collect();
        let passages = split(&text);
        assert_eq!(passages.len(), 10);
        assert_eq!(passages[6].number, 7);
        assert_eq!(rank(&passages, "PLATYPUS aqueduct", 3)[0].number, 7);
        assert!(rank(&passages, "absentterm", 3).is_empty());
        assert_eq!(
            passages.iter().map(|p| p.text.as_str()).collect::<String>(),
            text
        );
        assert!(
            passages
                .iter()
                .all(|p| p.text.split_whitespace().count() <= 301)
        );
    }

    #[test]
    fn long_tokens_and_unicode_offsets_are_bounded() {
        let text = "🐾".repeat(10_000);
        let passages = split(&text);
        assert_eq!(passages.len(), 5);
        assert!(passages.iter().all(|p| p.text.chars().count() <= MAX_CHARS));
        assert_eq!(passages.last().unwrap().end_offset, 10_000);
        assert_eq!(overlapping(&passages, 2399, 2401).len(), 2);
    }
}
