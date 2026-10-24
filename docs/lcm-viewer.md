# LCM viewer

Status: built, updated 2026-10-21.

Updated 2026-10-21 for the central store. A local, offline, read-only page for inspecting
a chat store: the original messages with the LCM episodes and chapters laid over them, the
summaries, the split decisions, the checkpoints, the frozen serving view, and for replay
output folders the probes and model cost.

Code: [tools/lcm-viewer/](../tools/lcm-viewer/) (`serve.py` and `index.html`). It is a
Python 3 stdlib server with a vanilla JS page, not part of the Rust engine.

## Run

```
python tools/lcm-viewer/serve.py
python tools/lcm-viewer/serve.py --data-dir D:\scratch\store-copy --port 8766
```

Then open `http://127.0.0.1:8766/`.

- With no argument it opens the machine's central store: `%LOCALAPPDATA%\BubblePaw-Core\store`
  on Windows, else `$XDG_DATA_HOME/BubblePaw-Core/store` or
  `~/.local/share/BubblePaw-Core/store` (the same lookup the engine uses). If only the
  previous `BubblePaw-base/store` exists, it opens that one.
- `--data-dir DIR` (repeatable, alias `--root`) replaces the default. `DIR` is a store
  (it holds `catalog.db` and `chats/`) or a folder that contains stores up to four levels
  down, such as a replay output folder (its store is `<out>/store`). Extra positional
  arguments add single `chats/*.db` files.
- Optional: `pip install zstandard` (Python 3.14 has `compression.zstd` built in). Message
  bodies over 4 KiB, checkpoint summaries and cold rows are zstd-compressed in the chat
  file, and archived chats are `*.db.zst`. Without zstd the viewer still runs, and those
  contents appear as placeholders that name the blob and its size.

## Store layout it reads

See [storage-model.md](storage-model.md). Each `chats/<root-id>.db` is one source (a root
chat with its sub-agent sessions; the session picker lists them with titles).

| File | Used for |
| --- | --- |
| `catalog.db` `sessions` | chat titles and storage state |
| `chats/<root>.db` `sessions`, `messages`, `blobs`, `message_dictionaries` | transcript; blob and cold-row decoding; `root_workspace:<root>` metadata for the workspace |
| `chats/<root>.db` `context_episodes`, `context_subject_chapters`, `context_decisions`, `context_chapters`, `context_checkpoints` | the LCM structure, with checkpoint summaries read from their blob |
| `serving.db` `context_views` | the frozen serving view. It is disposable, so a chat may have none |

The serving view is stored as pointers (message ranges and text slices of episodes,
chapters and checkpoints). The viewer expands them into the prefix messages the way the
engine does; generated messages are marked `generated`.

## What it shows

- **Sources** (left): every chat in the store, grouped by store, with title, message,
  episode, chapter and decision counts, schema version, workspace folder and an
  `archived` tag. Sources are named by id, never by a path sent to the page.
- **Timeline** (middle): each message with seq, role, origin and content. Long content
  collapses, and tool calls and results start collapsed. A coloured gutter marks the
  episode, and episode and chapter headings appear where they start. A text filter
  covers content and tool calls.
- **Details** (right): the episode, chapter, message or decision selected. Episode details
  show the source range, chapter, summary, split decision (probability of each option and
  of "none") and a raw JSON toggle. Selecting an episode scrolls the timeline to its first
  message.
- **Tabs** (shown when the data exists): Episodes & chapters (with legacy chapters and
  classical checkpoints), Final view (the expanded serving view, and `final-view.txt` or
  `view-*.txt` of a replay), Probes, Run report (`report.json` summary, totals, and model
  calls aggregated from `calls.jsonl`). The replay tabs appear for a store at
  `<out>/store` whose parent holds replay output.
- The URL hash holds the source id, session, tab, and selected seq or episode, so a reload
  keeps the view.

## API (GET only)

| Path | Returns |
| --- | --- |
| `/api/sources[?rescan=1]` | roots, chats with counts and session ids |
| `/api/sources/<id>` | one chat summary |
| `/api/sources/<id>/sessions/<session>` | messages, episodes, chapters, legacy chapters, checkpoints, decisions, expanded view, warnings |
| `/api/sources/<id>/replay` | report summary, probes, call aggregates, view texts |

## Limits

- Read-only. Every database is opened `mode=ro` with `PRAGMA query_only`, and as
  `immutable=1` when it has no live WAL, so no `-wal` or `-shm` file appears. A live host
  has a WAL: SQLite then reads it and may update the existing `-shm`, which holds no store
  data. The viewer never opens `store.lock`, takes no lease and does not rebuild or
  migrate anything. A running host is unaffected, and the page shows the committed state at
  load; use `Rescan` and reload to refresh. Archived chats are decompressed in memory.
- To inspect a store without any chance of interference, point `--data-dir` at a copy.
- Binds 127.0.0.1 only, and rejects requests whose Host header is not 127.0.0.1 or
  localhost.
- Counts are cached by file mtime and size (including the WAL).
- Chats whose messages are stored as attachment parts (`_content_parts*`) show the stored
  fields with a note, not the rendered content.
- Text views are capped at 4 MB per file. Message content is not capped on the server.
- The split-decision probability list is index-aligned: the last entry is "none" when
  there is one more probability than options, as in the episode design kept in the project notes.
- Probe verdicts are read from `judge.judgment.grade`. Probes with another shape show as
  ungraded.
