# Child worktrees

Status: built 2026-10-15; acceptance uses scripted providers and local git
repositories. See [verification](verification.md) for the checks that run.

`tools.agent({task, profile: "workspace", worktree: true})` creates a git worktree
at `<BubblePaw data dir>/worktrees/<child-session-id>` on `bp/<child-session-id>`.
The source is the direct parent's current workspace HEAD. Parent uncommitted
changes are not copied; the spawn receipt states this explicitly. A workspace
without a git repository or commits returns a clear admission error.

The default is `worktree: false`, sharing the parent's workspace. A descendant
inherits an isolated parent's workspace unless it requests another worktree;
that worktree branches from the isolated parent's HEAD. All five file tools (`read`, `list`, `search`, `edit`, `write`), exec, JS
artifact captures and memory-file discovery use the effective child workspace.
The receipt and child status include `workspace`, `branch` and `worktree.path`.
Tree listings preserve those fields and add `parentSessionId`.

With [host attach](host-attach.md), each root chat owns a persisted workspace.
Workspace resolution starts from that binding, including after catalog rebuild;
it does not use the process host's workspace for an unrelated chat. A descendant
still follows its nearest isolated ancestor, and a new nested worktree still
branches from that ancestor's HEAD. Protected roots consist of its own root chat
workspace and its effective child worktree. Legacy catalog workspace bindings
remain the fallback for old chats, without rewriting their files. A malformed
binding (a relative path, or a key naming another root) rejects only that chat:
catalog rebuild skips it with a stderr notice naming the chat, leaves its file
unchanged, and keeps the rest of the store usable.

Skills are discovered from the effective child workspace. External skill read
grants remain keyed by session and are shared across the host's workspace
adapters. Native command and JavaScript spills stay under the BubblePaw data
directory, with the same session ownership, retention and deletion behavior as
the parent host; process views retain the shared host limits.

Worktrees remain after success, cancellation and timeout because ordinary children
are continuable. The runner's archive and delete commands clean inactive bindings
after the SQLite operation succeeds. Automatic housekeeping is reconciled at host
startup, child completion and live-config turn boundaries. Dirty worktrees,
including untracked or ignored files, stay in place. A parent workspace needed by
a retained or active descendant stays too. Branches with commits absent
from the direct parent's HEAD also stay in place. Cleanup reports each outcome;
git removal and branch deletion never use force.

SQLite attachments retain the authoritative workspace binding without a schema
change. A small JSON lifecycle mirror beside the worktrees permits cleanup after
chat deletion and avoids reopening archived SQLite files. It also distinguishes
worktrees deliberately removed by cleanup from unexpected missing paths. An
explicit continuation restores only a deliberately removed worktree; a missing
or moved worktree fails closed. Restoring nested worktrees restores their parent
workspaces first. A clean removed branch can be recreated from the parent's
current HEAD; unmerged branches are never removed.

This is git workspace separation. Approval and profile restrictions continue to
apply through the ordinary child harness. Internal host-created worker profiles
cannot request worktree isolation; model-facing registered profiles can.
