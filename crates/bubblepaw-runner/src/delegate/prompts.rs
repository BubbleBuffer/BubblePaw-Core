//! Prompts for a delegated worker: the first assignment message and the rework message.
use super::{assignment::Assignment, checks::CheckResult};

/// The first user message: the body, the `read_first` section (empty when none), then the
/// report instructions and the checks. Rework resumes do not use this.
pub(super) fn first_prompt(assignment: &Assignment, read_first: &str) -> String {
    let mut prompt = assignment.body.clone();
    if !read_first.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(read_first);
    }
    prompt.push_str("\n\nThe workspace may be a linked git worktree that shares its stash with other runs: never use `git stash`. To set work aside or compare with a baseline, use `git diff` or a temporary `git worktree add --detach`.");
    prompt.push_str("\n\n---\nWhen you have finished, call `submit_result` with the delegation report: `status` (done, failed or blocked), `summary`, `checks_claimed` (each command you ran to verify the work, with whether it passed) and `open_questions`. In JS mode, use `tools.submit_result(...)`.");
    if !assignment.checks.is_empty() {
        prompt.push_str(
            " After you stop, the harness runs these acceptance checks in the workspace itself; each must exit 0:\n",
        );
        for check in &assignment.checks {
            prompt.push_str(&format!("- `{check}`\n"));
        }
    }
    prompt
}

pub(super) fn rework_prompt(failing: &[&CheckResult]) -> String {
    let mut prompt = String::from(
        "The harness ran the acceptance checks and these failed. Fix the work, then call `submit_result` with a new final report.\n",
    );
    for check in failing {
        prompt.push_str(&format!(
            "\n$ {}\n{}\n```\n{}\n```\n",
            check.command,
            check.line(),
            check.tail
        ));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_prompt_forbids_git_stash_in_shared_worktrees() {
        let assignment = Assignment::parse(
            "+++\ncategory = \"implement\"\ntags = []\nchecks = []\nrework = 0\nroute = { class = \"small\" }\n+++\nDo the work.\n",
        )
        .unwrap();
        let prompt = first_prompt(&assignment, "");
        assert!(prompt.contains("linked git worktree that shares its stash"));
        assert!(prompt.contains("never use `git stash`"));
        assert!(prompt.contains("git worktree add --detach"));
    }
}
