# Skills

Status: Built 2026-10-15; nine focused checks and ten configured-host checks pass
on Linux with Rust 1.97.1. Full workspace gates are recorded in
[verification](verification.md). No live-model claim.

A skill is a folder containing `SKILL.md` with YAML frontmatter:

```markdown
---
name: release-checks
description: >-
  Prepare and check a release of this project.
---
Follow the project's release steps here.
```

`name` and `description` must be nonempty strings. Quoted and multiline YAML
strings work; other frontmatter fields are allowed. Invalid, missing or
unreadable frontmatter skips that skill and produces a bounded reminder.
Frontmatter is limited to 64 KiB. Bodies are hashed for change detection but are
never loaded into the system prompt or saved in the skill attachment.

Discovery starts at the canonical workspace and walks up to the nearest Git root
(a `.git` directory or worktree `.git` file). At each level `.agents/skills`
comes before `.claude/skills`; nearer workspace directories come before their
parents. Without Git, only the workspace is searched. Next comes the user's
`~/.config/bubblepaw/skills`, then explicitly configured directories in order.
Each collection contains immediate skill folders, sorted by folder name. An
explicit directory can itself be a skill folder. The first valid skill with a
given name wins. This user directory intentionally differs from the older
`~/.bubblepaw` TOML and memory-file location.

```toml
[skills]
enabled = true # default
dirs = ["../team-skills"] # default []; paths relative to the owning config file
```

Both settings apply live at the next turn. CLI/environment relative paths use
the workspace as their base. No skill tool, command expansion, imports, plugin
loader or executable hooks are added.

## Frozen catalog and live changes

Each session freezes its initial list of names, descriptions and absolute
`SKILL.md` paths in a small immutable SQLite attachment. The system prefix lists
complete entries under an 8 KiB UTF-8 budget, with an explicit truncation marker;
skills omitted by that cap are omitted from the available catalog. Bodies remain
ordinary files: use `tools.read({path: "/absolute/skill/SKILL.md"})` through `js`.
Supporting files inside a listed skill folder are readable through the same
tool. Skill instructions do not change approval policy.

The existing [live-config](live-config.md) turn-start check uses the same mtime
and size helper as [memory files](memory-files.md). It stats candidate files and
directories, including missing candidates. Changed catalogs produce one durable
diff with added, removed and changed names, current metadata and bounded skip
warnings. Body-only edits count as changes; an identical rewrite does not.
The original system prefix stays frozen. Reopening a session restores its initial
catalog and replays durable diffs to avoid delivering the same change again.
Each child freezes its own catalog at its own first turn.
Isolated children discover project skills from their worktree. Rebound file
adapters share the host's session-keyed external read grants, so listed external
skills remain readable without granting access to another session's spill files.

Read grants are per session and restricted to canonical descendants of folders
listed initially or announced later. A frozen or previously announced folder
remains authorized after its removal from the live catalog. A different session
does not inherit that grant. Canonical resolution denies symlink escapes and
unrelated sibling folders. `list`, `search`, `edit` and `write` retain their normal
workspace boundary; skills add only ordinary `read` access.

Even initial absence or disabled skills produce an empty frozen attachment, so
enabling skills after a host restart cannot rewrite an existing session's system
prefix. Unchanged turns add no skill messages, attachments or application write
transactions. No schema is added. `tests/skills.rs` covers discovery precedence,
duplicates, YAML errors, the byte cap, body edits, exact-once durable notices,
reopen behavior and read boundaries. `tests/live_config.rs` covers the configured
host, CodeMode reads of external skill bodies and helpers, live additions and
removal, a stable system prefix and a per-chat storage measurement.

Reproduce the measurement with `cargo +1.97.1 test -p bubblepaw-runner --test
live_config skills_scripted_per_chat_storage_measurement -- --nocapture`.

The measured catalog attachment is 237 bytes versus 45 bytes for an empty
disabled catalog; the roughly 22 KiB body is absent. Both two-turn fixtures end
at 405,504 database bytes and 379,136 WAL bytes. Each first turn uses five
application transactions, including one frozen skill attachment, and each
unchanged second turn uses three ordinary transactions. See verification for
the WAL deltas and baseline details.
