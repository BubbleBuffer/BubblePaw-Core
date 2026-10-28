# Workspace file tools

**Status:** scratchpad grants added 2026-10-28; revised 2026-10-20: literal `edit` and whole-file `write` replace the
former patch tool. Current checks and earlier dated acceptance are recorded
separately in [verification](verification.md); no live acceptance is claimed here.

The runner exposes five file tools in native mode and through optional `js`:
`read`, `list`, `search`, `edit`, and `write`. Read/write/edit accept the canonical
workspace root and the chat/project scratchpads, with read-only exceptions for
this chat's spill output and its listed skill folders.
These checks are tool boundaries, not an OS sandbox for commands. No prior read
or read hash is required. In JS mode, call them as `tools.NAME(...)`.

`tools.list({path?, limit?})` lists a directory in name order. The path defaults
to `.`, and the limit defaults to 200, with a maximum of 1000. Entries identify
files, directories and symlinks; truncation is explicit.

`tools.read({path, startLine?, lineCount?})` returns bounded UTF-8 text with
`content`, `startLine`, `totalLines`, `returnedLines`, and `truncated`. Line
numbers start at 1. The default is 1000 lines, with a maximum of 2000; the text
preview is limited to 64 KiB, and the serialized result has a separate 60 KiB
limit. Ordinary text files are limited to 8 MiB.

Absolute paths under `<data_dir>/spill/<chat-id>/` are readable only by that
chat session. `chat-id` is its session ID, including for child sessions; each
session has its own folder and 256 MiB retention allowance. Canonical resolution rejects other chats, traversal into their folders and
symlinks escaping the grant. Spill reads support the same line ranges up to the
256 MiB spill retention ceiling, scanning with bounded buffers so a large file
does not enter a JS heap or a SQLite blob. The retained reference can become
unavailable after oldest-first eviction. The read exception does not grant
external `list`, `search`, `edit` or `write` access. See [CodeMode](codemode.md) for returned
output references and limits.

Through the host dispatcher, `read` recognizes PNG, JPEG, GIF and WebP by their
bytes and attaches supported images to model context. The limit is 5,000,000
bytes per image (decimal 5 MB), without resizing. One `js` call attaches at most
four distinct images; duplicate reads attach the same image once. Oversized
images return an explanatory note. See [providers](providers.md) for model input
behavior and [storage](storage-model.md) for image blobs.

`tools.search({query, path?, regex?, maxResults?})` searches workspace text.
Queries are literal unless `regex` is true. The default maximum is 100 results,
capped at 1000. Search skips symlinks, `.git`, `node_modules`, `target`, binary
files and files over 2 MiB. Match text and traversal are bounded; truncation is
explicit.

## Chat and project scratchpads (2026-10-28)

`read`, `write` and `edit` accept both scratchpad paths shown in the prompt,
as well as the workspace root. Chat files live under
`<data_dir>/scratch/chat/<root-chat-id>/`; children share this directory and use
a subfolder named after their own chat ID. Project files live under
`<data_dir>/scratch/workspace/<SHA-256 of canonical workspace root>/` and are
shared by chats using that workspace.

Use absolute paths for scratch files. A `..` escape, a symlink or a Windows
junction resolving outside the selected scratchpad is refused, including a
link back into the workspace or into the other scratchpad. Other chats'
scratchpads are not granted even when the data directory is inside the workspace.
Windows accepts ordinary and verbatim absolute paths. Scratch mutation results
return absolute paths; workspace mutation results remain workspace-relative.

Write a script with `write`, then run it with argv `exec` in the same batch.
Scratch `write` and `edit` retain ordinary tool policies, atomic replacement and
mutation locking. Their disposable files are excluded from workspace rewind
checkpoints. Root-chat deletion removes the chat scratchpad; project retention
is described in [storage](storage-model.md#scratchpads-2026-10-28).
No new approval or configuration setting is needed. `list` and `search` keep
their workspace boundary.

## File mutations

```js
await tools.edit({
  path: "README.md",
  old_string: "Old description.",
  new_string: "New description."
});
return await tools.write({path: "nested/new.txt", content: "new file\n"});
```

`tools.edit({path, old_string, new_string, replace_all?})` replaces literal text
in an existing UTF-8 file. `old_string` must be nonempty and differ from
`new_string`. Matching preserves every byte of whitespace, Unicode and line
endings: there is no regex, fuzzy matching or normalization. LF and CRLF differ.
Exactly one match is required unless `replace_all: true` selects every
non-overlapping occurrence. The result is exactly `{path, replacements}`.
For several edits, call `edit` several times; each call has its own commit and
checkpoint boundary.

A zero-match error names the file, shows up to three nearest numbered file lines
based on the first line of `old_string`, adds a difference hint when available and
advises a fresh `read`. A multiple-match error gives the match count and first
matching line locations; use more unique surrounding text or deliberate
`replace_all`. A missing-file error names the path and advises `write` to create
it. Errors are diagnostic only; they never relax exact matching.

`tools.write({path, content})` creates or overwrites a UTF-8 file and creates
missing parent directories. Its result is exactly `{path, bytes, created}`:
`bytes` is the UTF-8 byte count, and `created` identifies a newly created file.
Content is written exactly as supplied, including line endings and final newline.
Neither mutation result contains checkpoint metadata. For workspace files, returned paths are canonical
workspace-relative paths using `/` between directories on every platform; a
literal backslash in a Unix filename remains part of its name.

Both mutations validate paths, input, encoding and limits before changing the
target. Targets must be regular files or, for `write`, absent; symlinks are
rejected. Original and resulting files have a 32 MiB limit. The shared workspace
mutation lock serializes this runner's own mutations. Replacement content is
staged, and the original bytes or absence are compared again immediately before
commit. An intervening outside change aborts the write. Existing permissions
are retained, and new-file installation does not clobber a racing creation.

The staged file replacement is atomic, but filesystem and SQLite commits are
separate. An external writer can race the final check and replacement. An I/O
failure, cancellation or process interruption can leave new directories or a
committed file whose tool result was not delivered. Inspect the file after an
unknown outcome; do not automatically replay an external effect.

For workspace files, both `edit` and `write` durably checkpoint changed files before target writes,
using the existing content-addressed attachment blobs. Originals and final
content up to 10 MiB are covered. Larger changes remain allowed within the
32 MiB file limit and receive a durable barrier that prevents rewind across the
change. The compact tool results do not expose checkpoint fields. Unchanged
writes need no checkpoint. Session deletion prunes checkpoints; archives keep
them. `/rewind <message-id>` forks history and restores only matching later
changes owned by the source session. Exec and user edits are not covered.
Existing records from the former patch tool remain usable. See
[rewind](rewind.md) for conflicts, retention and host entry points.
Direct low-level invocation without a dispatcher is rejected before mutation.

`edit` and `write` retain the ordinary dispatcher, approval gate and workspace
containment. Read-only hosts omit both, and approval waits or denials occur before
mutation work. Per-tool approval rules must name `edit` and `write` to cover both
mutation capabilities. Both signatures are always shown in the JS tool description when
available. Child workspaces rebind both tools to the effective child checkout,
alongside `read`, `list` and `search`.
See [approvals](approvals.md), [child worktrees](child-worktrees.md) and
[CodeMode](codemode.md).

Unknown argument names return the valid argument names. Missing-path errors
identify the requested path. These defaults were updated on 2026-10-20.
