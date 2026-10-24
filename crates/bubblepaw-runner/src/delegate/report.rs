//! The worker's schema-validated final report and false-green detection.
use super::checks::CheckResult;
use anyhow::{Context, Result};
use bubblepaw_core::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClaimedCheck {
    pub command: String,
    pub passed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinalReport {
    /// done, failed or blocked.
    pub status: String,
    pub summary: String,
    pub checks_claimed: Vec<ClaimedCheck>,
    pub open_questions: Vec<String>,
}

pub fn schema() -> JsonSchema {
    JsonSchema::new(
        "delegation_report",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["status", "summary", "checks_claimed", "open_questions"],
            "properties": {
                "status": {"type": "string", "enum": ["done", "failed", "blocked"]},
                "summary": {"type": "string"},
                "checks_claimed": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["command", "passed"],
                        "properties": {
                            "command": {"type": "string"},
                            "passed": {"type": "boolean"}
                        }
                    }
                },
                "open_questions": {"type": "array", "items": {"type": "string"}}
            }
        }),
    )
    .expect("delegation report schema is a closed object")
}

impl FinalReport {
    /// The final reply text, already validated by the harness against [`schema`].
    pub fn parse(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text.trim()).context("final report is not JSON")?;
        let report: Self = serde_json::from_value(value).context("final report shape")?;
        anyhow::ensure!(
            matches!(report.status.as_str(), "done" | "failed" | "blocked"),
            "final report status must be done, failed or blocked"
        );
        Ok(report)
    }
}

/// One recorded exec in the worker session, in call order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedExec {
    pub command: String,
    /// None while still running or when no exit code was recorded.
    pub exit_code: Option<i64>,
}

fn same_command(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// Claims of passing checks contradicted by recorded facts. A claim counts once when
/// the harness ran the same command and it failed, or when the session's last recorded
/// exec of that command exited non-zero.
pub fn false_greens(
    report: Option<&FinalReport>,
    harness: &[CheckResult],
    execs: &[RecordedExec],
) -> Vec<String> {
    let Some(report) = report else {
        return Vec::new();
    };
    report
        .checks_claimed
        .iter()
        .filter(|claim| claim.passed)
        .filter(|claim| {
            let harness_failed = harness
                .iter()
                .any(|check| same_command(&check.command, &claim.command) && !check.passed());
            let last_exec_failed = execs
                .iter()
                .rev()
                .find(|exec| same_command(&exec.command, &claim.command))
                .is_some_and(|exec| exec.exit_code.is_some_and(|code| code != 0));
            harness_failed || last_exec_failed
        })
        .map(|claim| claim.command.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(claims: &[(&str, bool)]) -> FinalReport {
        FinalReport {
            status: "done".into(),
            summary: "ok".into(),
            checks_claimed: claims
                .iter()
                .map(|(command, passed)| ClaimedCheck {
                    command: (*command).into(),
                    passed: *passed,
                })
                .collect(),
            open_questions: vec![],
        }
    }
    fn check(command: &str, exit: i32) -> CheckResult {
        CheckResult {
            command: command.into(),
            exit_code: Some(exit),
            duration_ms: 1,
            tail: String::new(),
            timed_out: false,
        }
    }
    fn exec(command: &str, exit: i64) -> RecordedExec {
        RecordedExec {
            command: command.into(),
            exit_code: Some(exit),
        }
    }

    #[test]
    fn claimed_pass_failing_the_harness_run_is_a_false_green() {
        let r = report(&[("cargo test", true), ("cargo fmt", true), ("lint", false)]);
        let harness = [
            check("cargo  test", 101),
            check("cargo fmt", 0),
            check("lint", 1),
        ];
        assert_eq!(false_greens(Some(&r), &harness, &[]), ["cargo test"]);
    }

    #[test]
    fn claimed_pass_whose_last_recorded_exec_failed_is_a_false_green() {
        let r = report(&[("make a", true), ("make b", true), ("make c", true)]);
        let execs = [
            exec("make a", 0),
            exec("make a", 2),
            exec("make b", 2),
            exec("make b", 0),
            RecordedExec {
                command: "make c".into(),
                exit_code: None,
            },
        ];
        // a: last run failed; b: fixed on its last run; c: no exit recorded.
        assert_eq!(false_greens(Some(&r), &[], &execs), ["make a"]);
        // Both rules on one claim count it once.
        let both = report(&[("make a", true)]);
        assert_eq!(
            false_greens(Some(&both), &[check("make a", 1)], &execs).len(),
            1
        );
        assert!(false_greens(None, &[check("make a", 1)], &execs).is_empty());
    }

    #[test]
    fn report_schema_is_closed_and_parsing_rejects_unknown_fields() {
        let schema = schema();
        assert_eq!(schema.schema["additionalProperties"], false);
        let ok = r#"{"status":"done","summary":"s","checks_claimed":[{"command":"x","passed":true}],"open_questions":[]}"#;
        assert_eq!(FinalReport::parse(ok).unwrap().checks_claimed.len(), 1);
        assert!(
            FinalReport::parse(
                r#"{"status":"maybe","summary":"s","checks_claimed":[],"open_questions":[]}"#
            )
            .is_err()
        );
        assert!(
            FinalReport::parse(
                r#"{"status":"done","summary":"s","checks_claimed":[],"open_questions":[],"x":1}"#
            )
            .is_err()
        );
        assert!(FinalReport::parse("done!").is_err());
    }
}
