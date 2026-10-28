//! Digest of the runner sources and prompts that shape recorded host behaviour.

pub(crate) fn source_fingerprint() -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for source in [
        include_str!("main.rs"),
        include_str!("source_fingerprint.rs"),
        include_str!("root_prompt.rs"),
        include_str!("host_attach.rs"),
        include_str!("host_lifetime.rs"),
        include_str!("host_attach/io.rs"),
        include_str!("host_transport.rs"),
        include_str!("session_host.rs"),
        include_str!("session_scope.rs"),
        include_str!("cli_output.rs"),
        include_str!("config.rs"),
        include_str!("codemode.rs"),
        include_str!("input_files.rs"),
        include_str!("agents.rs"),
        include_str!("agents_question.rs"),
        include_str!("agent_handles.rs"),
        include_str!("episodes.rs"),
        include_str!("episode_models.rs"),
        include_str!("episode_fork.rs"),
        include_str!("history_snapshot.rs"),
        include_str!("model_roles.rs"),
        include_str!("provider/registry.rs"),
        include_str!("provider/live_provider.rs"),
        include_str!("images.rs"),
        include_str!("provider/endpoints.rs"),
        include_str!("provider/decisions.rs"),
        include_str!("provider/chat.rs"),
        include_str!("episode_worker.rs"),
        include_str!("episode_background.rs"),
        include_str!("summary_prompt_v2.txt"),
        include_str!("summary_prompt_v3.txt"),
        include_str!("chapter_prompt_v1.txt"),
        include_str!("summarization.rs"),
        include_str!("summary_prompt.md"),
        include_str!("runtime_note.rs"),
        include_str!("command_template.rs"),
        include_str!("trace.rs"),
        include_str!("todo.rs"),
        include_str!("question.rs"),
        include_str!("question_child.rs"),
        include_str!("question_stdio.rs"),
        include_str!("tools/process.rs"),
        include_str!("../../bubblepaw-core/src/lib.rs"),
        include_str!("../../bubblepaw-core/src/runtime.rs"),
        include_str!("../../bubblepaw-core/src/dispatch.rs"),
        include_str!("../../bubblepaw-core/src/decision.rs"),
        include_str!("../../bubblepaw-core/src/approvals/mod.rs"),
        include_str!("../../bubblepaw-core/src/approvals/modes.rs"),
        include_str!("../../bubblepaw-core/src/approvals/rules.rs"),
        include_str!("../../bubblepaw-core/src/observation.rs"),
        include_str!("../../bubblepaw-core/src/host_state.rs"),
    ] {
        digest.update(source.len().to_le_bytes());
        digest.update(source.as_bytes());
    }
    format!("{:x}", digest.finalize())
}
