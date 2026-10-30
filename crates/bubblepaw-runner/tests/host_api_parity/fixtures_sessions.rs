//! Fixtures for the session, history, todo, context and event methods. Each
//! fixture creates its own paired target so the transports never share state.
use super::fixtures::*;
use serde_json::json;

pub fn all() -> Vec<Fixture> {
    vec![
        fixture(
            "session.open",
            vec![
                open("open", "s"),
                ok("session.open", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "session.list",
            vec![
                open("list", "s"),
                ok("session.list", json!({})).sessions(),
                ok("session.list", json!({"limit": 1000})).sessions(),
            ],
        ),
        fixture(
            "session.rename",
            vec![
                open("rename", "s"),
                ok(
                    "session.rename",
                    json!({"session_id": "$s", "title": "parity-$role-renamed"}),
                ),
            ],
        ),
        fixture(
            "session.archive",
            vec![
                open("archive", "s"),
                ok("session.archive", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "session.unarchive",
            vec![
                open("unarchive", "s"),
                ok("session.archive", json!({"session_id": "$s"})),
                ok("session.unarchive", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "session.search",
            vec![
                open("search", "s"),
                run_text("s", "paritysearchtoken"),
                ok(
                    "session.search",
                    json!({"session_id": "$s", "query": "paritysearchtoken"}),
                )
                .contains("paritysearchtoken"),
            ],
        ),
        fixture(
            "session.fork",
            vec![
                open("fork", "s"),
                run_text("s", "parity fork"),
                ok(
                    "session.fork",
                    json!({"session_id": "$s", "title": "parity-$role-fork"}),
                ),
            ],
        ),
        fixture(
            "session.state",
            vec![
                open("state", "s"),
                run_text("s", "parity state"),
                ok("session.state", json!({"session_id": "$s"})).contains("parity state"),
            ],
        ),
        fixture(
            "session.history",
            vec![
                open("history", "s"),
                run_text("s", "parity history"),
                ok("session.history", json!({"session_id": "$s"})).contains("parity history"),
            ],
        ),
        fixture(
            "session.rewind",
            vec![
                open("rewind", "s"),
                run_text("s", "parity rewind"),
                ok("session.history", json!({"session_id": "$s"})).bind("m0", "/messages/0/id"),
                ok(
                    "session.rewind",
                    json!({"session_id": "$s", "message_id": "$m0"}),
                ),
            ],
        ),
        fixture(
            "session.compact",
            vec![
                open("compact", "s"),
                run_text("s", "parity compact"),
                ok("session.compact", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "session.delete",
            vec![
                open("delete", "s"),
                ok("session.delete", json!({"session_id": "$s"})).contains("deleted"),
            ],
        ),
        fixture(
            "todo.list",
            vec![
                open("todo", "s"),
                todo_run("s"),
                ok("todo.list", json!({"session_id": "$s"})).contains("parity todo"),
            ],
        ),
        fixture(
            "question.pending_child",
            vec![
                open("pending", "s"),
                ok("question.pending_child", json!({"session_id": "$s"})),
                open_child("pending-child", "s", "c"),
                question_run("c"),
                ok("question.pending_child", json!({"session_id": "$c"})).contains("Which colour?"),
                ok("run.cancel", json!({"session_id": "$c"})).idle("c"),
            ],
        ),
        fixture(
            "host.command",
            vec![
                open("command", "s"),
                // A recognized command runs against the selected session.
                ok(
                    "host.command",
                    json!({"session_id": "$s", "text": "/rename parity-$role-by-command"}),
                ),
                // Ordinary input is not a command, so it has no result.
                ok("host.command", json!({"text": "ordinary input"})),
            ],
        ),
        fixture(
            "session.view",
            vec![
                open("session-view", "s"),
                run_text("s", "parity session view"),
                ok("session.view", json!({"session_id": "$s"})).contains("parity session view"),
            ],
        ),
        fixture(
            "session.sidebar",
            vec![
                open("sidebar", "s"),
                ok("session.sidebar", json!({})).sessions(),
            ],
        ),
        fixture(
            "events.after",
            vec![
                open("events", "s"),
                // Zero predates the second host generation and needs resync.
                ok("session.state", json!({"session_id": "$s"})).bind("after", "/seq"),
                run_text("s", "parity events"),
                ok(
                    "events.after",
                    json!({"session_id": "$s", "after": "$after"}),
                ),
            ],
        ),
        fixture(
            "context.view",
            vec![
                open("view", "s"),
                run_text("s", "parity view"),
                ok("context.view", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "context.structure",
            vec![
                open("structure", "s"),
                run_text("s", "parity structure"),
                ok("context.structure", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "context.summary",
            vec![
                open("summary", "s"),
                run_text("s", "parity summary"),
                err(
                    "context.summary",
                    json!({"session_id": "$s", "kind": "episode", "id": "missing-episode"}),
                    "unknown or expired context summary",
                ),
            ],
        ),
        fixture(
            "context.events",
            vec![
                open("context-events", "s"),
                run_text("s", "parity context events"),
                ok("context.events", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "history.range",
            vec![
                open("range", "s"),
                run_text("s", "parity range"),
                ok("session.history", json!({"session_id": "$s"})).bind("m0", "/messages/0/id"),
                ok("session.history", json!({"session_id": "$s"})).bind("m1", "/messages/1/id"),
                ok(
                    "history.range",
                    json!({"session_id": "$s", "first_message_id": "$m0", "last_message_id": "$m1"}),
                ),
            ],
        ),
        fixture(
            "history.messages",
            vec![
                open("messages", "s"),
                run_text("s", "parity messages"),
                ok("history.messages", json!({"session_id": "$s", "limit": 10}))
                    .contains("parity messages"),
            ],
        ),
        fixture(
            "session.import",
            vec![
                // No transcript is shipped with the fixtures, so the import is
                // refused the same way on both transports.
                any_err(
                    "session.import",
                    json!({"path": "does-not-exist.jsonl", "format": "codex", "title": "parity-$role-import"}),
                ),
            ],
        ),
    ]
}
