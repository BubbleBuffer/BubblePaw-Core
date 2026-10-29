use super::*;

fn episodes() -> Vec<String> {
    (7..=10)
        .map(|ordinal| format!("EPISODE {ordinal} [source a..b]\nmemory"))
        .collect()
}
fn output(blocks: &str) -> String {
    format!("# Implementation\nThe parser fix passed at msg-1.\n\n{blocks}")
}

#[test]
fn parses_decisions_and_actual_non_one_based_ordinals() {
    let text = output(
        "Decisions:\n- Keep the parser fix.\n- Retain v1 by default.\n\nSections:\n- Parser fix | episodes 7-8\n- Verification | episodes 9-10",
    );
    let metadata = parse(&text, &episodes()).unwrap();
    assert_eq!(
        metadata.decisions,
        ["Keep the parser fix.", "Retain v1 by default."]
    );
    assert_eq!(
        metadata.sections,
        vec![
            ChapterSection {
                name: "Parser fix".into(),
                first_episode: 7,
                last_episode: 8
            },
            ChapterSection {
                name: "Verification".into(),
                first_episode: 9,
                last_episode: 10
            },
        ]
    );
    assert_eq!(
        prose(&text),
        "# Implementation\nThe parser fix passed at msg-1."
    );
}

#[test]
fn none_is_an_explicit_empty_decision_list() {
    let metadata = parse(
        &output("Decisions: none\nSections:\n- Parser work | episodes 7-10"),
        &episodes(),
    )
    .unwrap();
    assert!(metadata.decisions.is_empty());
    assert_eq!(metadata.sections.len(), 1);
}

#[test]
fn rejects_missing_empty_duplicate_reordered_and_malformed_blocks() {
    for blocks in [
        "",
        "Decisions: none",
        "Sections:\n- Work | episodes 7-10",
        "Decisions:\nSections:\n- Work | episodes 7-10",
        "Decisions: none\nSections:",
        "Decisions: none\n- Extra decision\nSections:\n- Work | episodes 7-10",
        "Sections:\n- Work | episodes 7-10\nDecisions: none",
        "Decisions: none\nDecisions: none\nSections:\n- Work | episodes 7-10",
        "Decisions: none\nSections:\n- Work | episodes 7-10\nSections:",
        "Decisions:\nNot a bullet\nSections:\n- Work | episodes 7-10",
        "Decisions:\n- \nSections:\n- Work | episodes 7-10",
        "Decisions: None\nSections:\n- Work | episodes 7-10",
        "Decisions: none\nSections:\n- Work | episode 7-10",
        "Decisions: none\nSections:\n- Work | episode 7",
        "Decisions: none\nSections:\n- Work | episodes +7-10",
        "Decisions: none\nSections:\n- Work | episodes 7-10\nExtra commentary",
    ] {
        assert!(
            parse(&output(blocks), &episodes()).is_err(),
            "accepted {blocks:?}"
        );
    }
}

#[test]
fn rejects_ranges_without_exact_contiguous_coverage() {
    for rows in [
        "- Work | episodes 1-4",
        "- Work | episodes 7-9",
        "- Work | episodes 8-10",
        "- Work | episodes 10-7",
        "- Work | episodes 7-11",
        "- Work | episodes 7-8\n- More | episodes 8-10",
        "- Work | episodes 7-7\n- More | episodes 9-10",
        "- Work | episodes 9-10\n- More | episodes 7-8",
        "- Work | episodes 7-10\n- More | episodes 10-10",
        "- Work | episodes 7-8\n- More | episode 8\n- Rest | episodes 9-10",
        "- Work | episodes 7-8\n- More | episode 10",
        "- Work | episodes 7-99999999999999999999999999999999999999",
    ] {
        let text = output(&format!("Decisions: none\nSections:\n{rows}"));
        assert!(parse(&text, &episodes()).is_err(), "accepted {rows:?}");
    }
    let noncontiguous = vec![
        "EPISODE 7 [source a..b]".into(),
        "EPISODE 9 [source c..d]".into(),
    ];
    assert!(
        parse(
            &output("Decisions: none\nSections:\n- Work | episodes 7-9"),
            &noncontiguous
        )
        .is_err()
    );
    assert!(
        parse(
            &output("Decisions: none\nSections:\n- Work | episodes 7-10"),
            &[]
        )
        .is_err()
    );
}

#[test]
fn enforces_character_and_word_limits() {
    for decision in ["a".repeat(100), "猫".repeat(100)] {
        assert!(
            parse(
                &output(&format!(
                    "Decisions:\n- {decision}\nSections:\n- Work | episodes 7-10"
                )),
                &episodes()
            )
            .is_ok()
        );
    }
    let long = output(&format!(
        "Decisions:\n- {}\nSections:\n- Work | episodes 7-10",
        "猫".repeat(101)
    ));
    assert!(parse(&long, &episodes()).is_err());
    for name in ["", "One two three four five six", "Work | extra"] {
        assert!(
            parse(
                &output(&format!(
                    "Decisions: none\nSections:\n- {name} | episodes 7-10"
                )),
                &episodes()
            )
            .is_err()
        );
    }
    assert!(
        parse(
            &output("Decisions: none\nSections:\n- One two three four five | episodes 7-10"),
            &episodes()
        )
        .is_ok()
    );
}

#[test]
fn malformed_suffix_keeps_only_ordinary_memory_without_inventing_a_body() {
    for blocks in [
        "Decisions: broken",
        "Sections:\n- Work | episodes 1-1",
        "Decisions: none\nSections:\n- Work | episodes 1-1",
    ] {
        let text = output(blocks);
        assert!(parse(&text, &episodes()).is_err());
        assert_eq!(
            prose(&text),
            "# Implementation\nThe parser fix passed at msg-1."
        );
        let empty = format!("# Empty\n\n{blocks}");
        assert_eq!(
            super::super::summary_output_reason("chapter", prose(&empty)),
            Some("empty_summary_body")
        );
    }
    let ordinary = "# Memory\nThe request mentions Decisions: but has no retrieval blocks.";
    assert_eq!(prose(ordinary), ordinary);
    assert!(parse(ordinary, &episodes()).is_err());
}

#[test]
fn related_final_line_is_parsed_before_retrieval_blocks() {
    let raw = format!(
        "{}\nRelated: none",
        output("Decisions: none\nSections:\n- Work | episodes 7-10")
    );
    let (clean, links, evidence) = super::super::parse_related_with_tasks(&raw, &[], &[], &[]);
    assert!(links.is_empty() && evidence.is_empty());
    assert!(parse(&clean, &episodes()).is_ok());
    assert!(!prose(&clean).contains("Related:"));
}

fn seven() -> Vec<String> {
    (1..=7)
        .map(|ordinal| format!("EPISODE {ordinal} [source a..b]\nmemory"))
        .collect()
}

#[test]
fn single_episode_row_parses_as_one_episode_range() {
    let text = output(
        "Decisions: none\nSections:\n- Reconnaissance and version design | episodes 1-4\n- Core numbered migration edits | episode 5\n- Fixtures, tests and docs | episodes 6-7",
    );
    let sections = parse(&text, &seven()).unwrap().sections;
    let ranges: Vec<_> = sections
        .iter()
        .map(|section| (section.first_episode, section.last_episode))
        .collect();
    assert_eq!(ranges, [(1, 4), (5, 5), (6, 7)]);
    assert_eq!(sections[1].name, "Core numbered migration edits");
}

#[test]
fn en_dash_range_parses() {
    let text = output(
        "Decisions: none\nSections:\n- Parser fix | episodes 7\u{2013}8\n- Rest | episodes 9\u{2013}10",
    );
    let sections = parse(&text, &episodes()).unwrap().sections;
    assert_eq!(sections.len(), 2);
    assert_eq!(
        (sections[0].first_episode, sections[0].last_episode),
        (7, 8)
    );
    assert_eq!(
        (sections[1].first_episode, sections[1].last_episode),
        (9, 10)
    );
}
