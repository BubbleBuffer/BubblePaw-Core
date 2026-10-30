//! Fixtures for run control, child control and question answers. A question is
//! always answered or cancelled inside its own fixture, so every run settles
//! before the next fixture starts.
use super::fixtures::*;
use serde_json::json;

pub fn all() -> Vec<Fixture> {
    vec![
        fixture(
            "run.start",
            vec![open("start", "s"), run_text("s", "parity start")],
        ),
        fixture(
            "run.resume",
            vec![
                open("resume", "s"),
                run_text("s", "parity resume"),
                // Admission does not require an interrupted run, so resume starts a
                // new run with no input on an idle session and succeeds.
                ok("run.resume", json!({"session_id": "$s"})).idle("s"),
            ],
        ),
        fixture(
            "run.steer",
            vec![
                open("steer", "s"),
                // Steering has no active-run guard: on an idle session the input is
                // accepted and stays pending, so this fixture does not wait for idle.
                ok(
                    "run.steer",
                    json!({"session_id": "$s", "text": "parity steer"}),
                ),
            ],
        ),
        fixture(
            "run.follow_up",
            vec![
                open("follow", "s"),
                question_run("s"),
                ok(
                    "run.follow_up",
                    json!({"session_id": "$s", "text": "parity follow up"}),
                ),
                ok(
                    "question.answer",
                    json!({"session_id": "$s", "question_id": "$question", "text": "Blue"}),
                )
                .idle("s"),
            ],
        ),
        fixture(
            "run.cancel",
            vec![
                open("cancel", "s"),
                question_run("s"),
                ok("run.cancel", json!({"session_id": "$s"})).idle("s"),
            ],
        ),
        fixture(
            "run.wait",
            vec![
                open("wait", "s"),
                run_text("s", "parity wait"),
                ok("run.wait", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "child.cancel",
            vec![
                open("child-cancel", "s"),
                // This session was never opened as a child of the parent, so the
                // cancel is refused. Managed-child success is covered by the stdio
                // children tests, not by this parity table.
                open("child-cancel-stranger", "x"),
                err(
                    "child.cancel",
                    json!({"parent": "$s", "child": "$x"}),
                    "agent belongs to another conversation",
                ),
                ok("session.delete", json!({"session_id": "$x"})),
            ],
        ),
        fixture(
            "question.answer",
            vec![
                open("answer", "s"),
                question_run("s"),
                ok(
                    "question.answer",
                    json!({"session_id": "$s", "question_id": "$question", "text": "Blue"}),
                )
                .idle("s"),
            ],
        ),
        fixture(
            "question.answer_child",
            vec![
                open("answer-child", "s"),
                open_child("answer-child-child", "s", "c"),
                question_run("c"),
                ok(
                    "question.answer_child",
                    json!({"parent": "$s", "child": "$c", "question_id": "$question", "text": "Green"}),
                )
                .idle("c"),
            ],
        ),
    ]
}
