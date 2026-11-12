use super::*;
use bubblepaw_core::ExecutionMode::{Js, Native};

/// A Native session has no REPL: its notes must not mention it, while Js notes keep
/// their exact wording (it is part of the cached prefix of existing Js sessions).
#[tokio::test]
async fn notes_use_the_wording_of_the_session_execution_mode() {
    let fixture = Fixture::new();
    let session = &fixture.session;
    let cases = [
        (
            Js,
            fixture.notes.note(session, Js),
            "REPL: no retained environments",
        ),
        (
            Js,
            fixture.notes.live_note(session, Js),
            "Runtime: no running commands, cells or agents.\n",
        ),
        (
            Native,
            fixture.notes.note(session, Native),
            "Commands: no retained handles",
        ),
        (
            Native,
            fixture.notes.live_note(session, Native),
            "Runtime: no running commands or agents.\n",
        ),
    ];
    for (mode, note, expected) in cases {
        assert!(note.contains(expected), "{mode:?}: {note}");
        let lower = note.to_lowercase();
        let repl_wording = ["repl", "cell", "tools."]
            .iter()
            .any(|word| lower.contains(word));
        assert_eq!(repl_wording, mode == Js, "{mode:?}: {note}");
    }
    fixture.shutdown().await;
}
