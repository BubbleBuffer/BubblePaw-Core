# Workspace file tools

**Status:** scratchpad grants added 2026-10-28; revised 2026-10-20: literal `edit` and whole-file `write` replace the
former patch tool. Current checks and earlier dated acceptance are recorded
separately in [verification](verification.md); no live acceptance is claimed here.

The runner exposes four file tools in native mode and through optional `js`:
`read`, `search`, `edit`, and `write`. Read/write/edit accept the canonical
workspace root and the chat/project scratchpads, with read-only exceptions for
this chat's spill output and its listed skill folders.
These checks are tool boundaries, not an OS sandbox for commands. No prior read
or read hash is required. In JS mode, call them as `tools.NAME(...)`.

`search` is the one tool for finding things (2026-11-27; it replaced `list` and the
old text-only `search`). `tools.search({path?, query?, glob?, regex?, context?,
filesOnly?, maxResults?})` walks `path` (default `.`) recursively in path order and honours
`.gitignore` (also outside a git repository); it skips symlinks and `.git`,
`node_modules` and `target`.

- **Files mode** (no `query`): the files under `path` with their sizes, so one
  call answers "what is in this repo". Default 500 files, maximum 5000.
- **Content mode** (`query`, literal text, or a regex with `regex:true`): the
  matching lines as `path:line: text`; `context:N` (maximum 10) adds `path-line- text`
  lines around each match, `filesOnly:true` returns only the files that contain
  a match. Default 100 matches, maximum 1000. Files over 2 MiB and non-UTF-8 files
  are skipped.
- `glob` is a `.gitignore`-style pattern or an array of them (`"*.rs"`,
  `["src/**/*.ts","!*.test.ts"]`); a leading `!` excludes. A file must match an
  include (when there is one) and no exclude. A `.gitignore`d file is never shown,
  whatever the glob says.
- A capped result ends with `[N more files not shown; ...]`; walks stop after
  100,000 paths and say so. The model sees plain text; `tools.search` in JS returns
  `{files:[{path,bytes}]|matches:[{path,line,text,context?}], more, truncated}`.

`read` takes several files in one call (2026-11-27): `paths` is an array of paths or
`{path,startLine,lineCount}` objects (a top-level `startLine`/`lineCount` is the
default for each). Each file comes back under a `=== path ===` header, in one
plain-text result under a single total cap (60 KiB less room for notes). A file that
does not fit is cut at a line boundary with a note giving the `startLine` to continue
from, files after it are skipped, and a closing `[total size cap reached. Cut: ...
Not read: ...]` line names both. A file that cannot be read (missing, outside the
grant) shows `[error: ...]` under its header without failing the others. Images
are read one at a time with `path`; at most 50 paths per call. `paths` and `path` are
exclusive.

When the model calls `read` with one `path`, the result is the file text for the range,
exactly as in the file. If it does not cover the whole file, a final line such as
`[lines 1-1000 of 2200; continue with startLine=1001]` says what was returned. An
empty file returns `[empty file]`; a `startLine` past the end returns
`[no lines: file has N lines]`. Text over the result limit is cut at a line
boundary and the note names the last line returned; a single over-long line is cut
at a character boundary (`[line N cut at B bytes; ...]`).

`tools.read({path, startLine?, lineCount?})` in JS keeps returning an object with
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
external `search`, `edit` or `write` access. See [CodeMode](codemode.md) for returned
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
No new approval or configuration setting is needed. `search` keeps
its workspace boundary.

## File mutations

```js
await tools.edit({
  path: "README.md",
  old_string: "Old description.",
  new_string: "New description."
});
return await tools.write({path: "nested/new.txt", content: "new file\n"});
```

`tools.edit({path, old_string, new_string, replace_all?})` or
`tools.edit({path, edits})` replaces literal text in an existing UTF-8 file.
`old_string` must be nonempty and differ from `new_string`. Matching ignores the
difference between CRLF and LF, in the file and in `old_string`, and ignores a
leading UTF-8 BOM; there is no regex, fuzzy matching or other normalization. The
write keeps the file's bytes otherwise intact: the BOM stays, untouched lines keep
their endings, and inserted text uses the file's majority line ending (LF on a
tie). Exactly one match is required unless `replace_all: true` selects every
non-overlapping occurrence. The result is exactly `{path, replacements}`.

`edits` is an array of `{old_string, new_string, replace_all?}` applied in order,
each to the result of the previous one, with the same rules per edit. The call is
all or nothing: if any edit fails, the file is not written and the error names the
failing edit by its zero-based index (`edits[1]: ...`). `replacements` is the total
across edits, and the call has one commit and checkpoint boundary. Giving `edits`
together with `old_string`, `new_string` or `replace_all` is an error.

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
alongside `read` and `search`.
See [approvals](approvals.md), [child worktrees](child-worktrees.md) and
[CodeMode](codemode.md).

Unknown argument names return the valid argument names. Missing-path errors
identify the requested path. These defaults were updated on 2026-10-20.
