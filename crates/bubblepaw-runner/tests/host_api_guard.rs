//! Keep transport adapters from acquiring their own host implementation again.
use std::{fs, path::Path};

fn transport(name: &str) -> bool {
    name == "main.rs"
        || name == "question_stdio.rs"
        || name.starts_with("stdio")
        || name.starts_with("web")
        || name.starts_with("cli")
}

fn scan(dir: &Path, failures: &mut Vec<String>) {
    let tests = regex::Regex::new(r"(?m)^#\[cfg\(test\)\]\s*\nmod ").unwrap();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan(&path, failures);
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "tests.rs" || name.ends_with("_tests.rs") {
            continue;
        }
        let web_module = path.components().any(|part| part.as_os_str() == "web");
        if path.extension().is_none_or(|extension| extension != "rs")
            || !(transport(name) || web_module)
        {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        // Comments describe the architecture and are not references to host APIs.
        let production = tests.split(&source).next().unwrap_or(&source);
        let code = production
            .lines()
            .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
            .collect::<Vec<_>>()
            .join("\n");
        for identifier in [
            "SessionControl",
            "Harness::start",
            "decide_from",
            "run_once_with",
            "QuestionTracker",
            "continue_answer",
            ".tracker(",
            ".answer_child_as_user(",
            ".start_resume(",
            ".admit(",
            ".cancel_owner(",
            ".background(",
            ".jobs(",
        ] {
            if code.contains(identifier) {
                failures.push(format!("{}: {identifier}", path.display()));
            }
        }
        for pattern in [
            r"\bharness\s*\.\s*(start|start_resume|steer_from|followup_from|cancel|cancel_from|decide|decide_from|wait_for_idle|pending_approvals|snapshot_with|events_after|subscribe)\s*\(",
            r"\bquestions\s*\.\s*(answer|pending|cancel|answer_child_as_user)\s*\(",
        ] {
            let expression = regex::Regex::new(pattern).unwrap();
            for found in expression.find_iter(&code) {
                failures.push(format!("{}: {}", path.display(), found.as_str()));
            }
        }
    }
}

#[test]
fn transports_only_call_the_host_method_table() {
    let mut failures = Vec::new();
    scan(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut failures,
    );
    assert!(
        failures.is_empty(),
        "transport bypasses host_api::dispatch:\n{}",
        failures.join("\n")
    );
}
