# Repository memory files

Status: Built 2026-10-15; scripted acceptance and storage measurements are recorded below. No live-model cache-hit claim.

The runner loads repository instructions into a session's system prompt. Discovery
reads the user config directory's `~/.bubblepaw/AGENTS.md` first. It then reads
`AGENTS.md` followed by `CLAUDE.md` at each directory from the nearest Git root
through the workspace, with outer directories first. Both a `.git` directory and
a worktree's `.git` file mark a root. Without Git, only the workspace is searched;
arbitrary ancestors outside a repository are excluded. Missing files are allowed.
Files with identical bytes appear once, and `@` imports are ordinary text.

The combined instruction prefix has a 32 KiB UTF-8 byte cap, including file
headings and a visible truncation marker. Discovery still tracks files after the
cap, and all candidate paths are statted even when they do not yet exist. File
hashing streams through bounded buffers, so an edit beyond the displayed cap is
detected without retaining an unlimited file body.

## Frozen sessions and live changes

At the first turn, one immutable session attachment stores the bounded initial
snapshot. Until compaction, every model request derives the same system prefix
from it; neither later file edits nor reopening the host rewrites that prefix. This also preserves
the initial absence of memory files. The host's base instructions and explicit
per-run system prompt remain the base, with the session instructions appended.

The existing [live-config](live-config.md) turn-start check stats memory-file paths
using the same mtime and size helper as TOML config files. A changed stamp triggers
content comparison; an identical rewrite sends no notice. Added, edited and
removed files produce one durable generated reminder per session on the next
model request, through the typed reminder queue. The JSON notice names paths and includes new
content; `content: null` means removal. A change has a fresh 32 KiB note budget,
so a file behind an unchanged full prefix can still deliver its new instructions.
Oversized changed text has `truncated: true`. Several changed files share that
budget. Original history and the frozen system prefix remain available.
At applied compaction (LCM or classical), the memory-file prefix is rebound from
current state. Covered change notices leave the serving view, and subsequent
diffs use the new snapshot as their baseline. The latest snapshot is durable;
reopening uses it and only later notices, while the original snapshot remains
available for history. Prefix bytes stay unchanged between compactions.

The host also stores one small initial prompt-binding attachment (base text,
component length and hash). It adds one initial transaction, including for
disabled/empty catalogs, without duplicating memory-file content. Apply stores
the latest snapshots and prompt binding in its existing canonical transaction;
a no-op compaction does not update them. The historical storage measurements
below predate this binding attachment.

The reminder is stored as an ordinary Generated message with `reminder_metadata`.
On process reopen the host seeds the last-sent hash for each kind from that
metadata, so content the session has already seen is not delivered again. Unchanged checks create no messages, attachments, application
transactions or WAL growth. No store schema is added. Core exposes a generic
session prompt resolver and atomic surface/view publication; discovery, snapshot
contents and notices remain runner code.

Each child has its own initial snapshot, loaded at its own first turn using its
host workspace. It does not copy the parent's frozen content. Current standard
child profiles share the parent's workspace, so a child started after an edit
sees the updated instructions while the parent retains its original prefix. A
host that binds a child to a separate workspace loads that workspace's files.

## Configuration

```toml
[context]
memory_files = false # default true; disabling requires a host restart
```

This switch disables discovery, loading, persisted snapshots and change notices.
It is restart-only, like the other context settings. Memory files supply model
instructions; they do not change [approval policy](approvals.md).

## Verification

`tests/memory_files.rs` covers user/repository order, both names, identical-content
deduplication, `.git` files, no-Git discovery, literal imports, UTF-8 truncation,
changes beyond the initial cap, add/remove, exact-once durable delivery across
store reopen, the disabled switch, separate child workspace snapshots and a
10 KiB session-store measurement. `tests/live_config.rs` uses a scripted provider
and real host wiring to check a stable root system prompt, durable change notices
and a new child's independent snapshot.

Linux focused acceptance passed on 2026-10-15 with Rust 1.97.1:
`--test memory_files` passed all nine discovery/cap/reopen/switch/storage checks;
`--test live_config` passed all eight host checks, including root/child freezing,
one-time change delivery and the two-turn measurement. Formatting, strict Clippy
and the full workspace suite are separate final integration gates.

The measured session uses deterministic varied 10 KiB instruction text, two real
harness turns with a scripted provider, and the standard per-chat data-directory
store. An otherwise equivalent session with `memory_files=false` is the baseline:

| Two-turn session | Database bytes | WAL bytes | Application transactions | Second-turn transactions |
|---|---:|---:|---:|---:|
| Memory enabled | 405,504 | 383,256 | 7 | 3 |
| Memory disabled | 405,504 | 346,176 | 6 | 3 |

These P1 totals predate the P2 frozen skill catalog. The combined host now adds
one skill snapshot transaction to either session, including an empty catalog;
the memory-only difference is unchanged. See [verification](verification.md) for the checks that run.

The initial snapshot adds one transaction and 37,080 WAL bytes; the unchanged
second turn adds no memory transactions beyond its three ordinary transactions.
The frozen attachment is 10,715 bytes and contains the instruction payload once.
It is absent from ordinary messages. Both model requests receive the same derived
system prefix. The enabled chat file is 262,144 bytes; preallocated pages absorb
the snapshot, so database and page growth were zero during this measurement.
These are local scripted storage results, with no live-model or cache-hit claim.

Reproduce the host measurement with `cargo +1.97.1 test -p bubblepaw-runner --test
live_config memory_files_ten_kib_scripted_session_storage_measurement -- --nocapture`.
The isolated snapshot-overhead measurement is `cargo +1.97.1 test -p
bubblepaw-runner --test memory_files ten_kib_snapshot_storage_measurement --
--nocapture`.
