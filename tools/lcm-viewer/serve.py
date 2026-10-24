#!/usr/bin/env python3
"""Read-only local viewer for BubblePaw chat stores and their LCM context.

A store is a directory with catalog.db, serving.db and chats/<root>.db. The
default is the machine's central store. Every SQLite file is opened read-only
(mode=ro with query_only, or immutable=1 when the file has no live WAL, so no
sidecar file is created). The viewer never opens or locks store.lock and never
writes. Clients name sources by id from /api/sources, never by path.

Usage:
  python tools/lcm-viewer/serve.py
  python tools/lcm-viewer/serve.py --data-dir /path/to/store --port 8766
  python tools/lcm-viewer/serve.py --data-dir storeA --data-dir storeB
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import hashlib
import json
import os
import re
import sqlite3
import sys
import threading
import time
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import quote, unquote, urlsplit

HERE = Path(__file__).resolve().parent
INDEX_HTML = HERE / "index.html"

MAX_SCAN_DEPTH = 4
SKIP_DIRS = {".git", "node_modules", "target", "chats", "legacy-import"}
REPLAY_FILES = ("report.json", "probes.json", "calls.jsonl", "final-view.txt")
MAX_TEXT_BYTES = 4_000_000
MAX_BLOB_BYTES = 64 * 1024 * 1024
ID_RE = re.compile(r"^[0-9a-f]{12}$")
VIEW_NAME_RE = re.compile(r"^(final-view\.txt|view-\d{3}\.txt)$")
REPORT_KEYS = (
    "chat", "mode", "status", "stop_reason", "partial", "context_strategy",
    "threshold", "context_window", "compaction_count", "messages_total",
    "messages_replayed", "max_cost_usd", "totals", "failures", "limitations",
    "final_view", "provenance", "recorded_model", "database", "session_id",
    "corpus_sha256",
)
EXPECTED_TABLES = (
    "sessions", "messages", "blobs", "context_episodes", "context_subject_chapters",
    "context_decisions", "context_chapters", "context_checkpoints",
)
NO_ZSTD = "zstd support missing: install the 'zstandard' package or use Python 3.14+"


def default_data_dir() -> Path:
    """The machine's central store, as the engine resolves it: BubblePaw-Core,
    or the previous BubblePaw-base folder when only that one exists."""
    base = os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_DATA_HOME")
    root = Path(base) if base else Path.home() / ".local" / "share"
    current = root / "BubblePaw-Core" / "store"
    previous = root / "BubblePaw-base" / "store"
    return previous if previous.is_dir() and not current.is_dir() else current


# ---------- optional zstd ----------

def _load_zstd():
    try:  # Python 3.14+
        from compression import zstd  # type: ignore

        def plain(data: bytes) -> bytes:
            return zstd.decompress(data)

        def with_dict(data: bytes, dictionary: bytes) -> bytes:
            return zstd.decompress(data, zstd_dict=zstd.ZstdDict(dictionary))

        return plain, with_dict
    except ImportError:
        pass
    try:
        import zstandard  # type: ignore

        def plain(data: bytes) -> bytes:
            return zstandard.ZstdDecompressor().decompressobj().decompress(data)

        def with_dict(data: bytes, dictionary: bytes) -> bytes:
            d = zstandard.ZstdCompressionDict(dictionary)
            return zstandard.ZstdDecompressor(dict_data=d).decompressobj().decompress(data)

        return plain, with_dict
    except ImportError:
        return None, None


ZSTD_PLAIN, ZSTD_DICT = _load_zstd()


@dataclass
class Source:
    id: str
    path: Path            # chat file (.db, or .db.zst when archived)
    store: Path           # store directory
    display: str
    group: str
    root_label: str
    title: str = ""
    workspace: str | None = None
    storage_state: str = "active"
    replay_dir: Path | None = None
    info: dict = field(default_factory=dict)


SOURCES: dict[str, Source] = {}
ROOTS: list[dict] = []
SCAN_LOCK = threading.Lock()
INFO_CACHE: dict[tuple, dict] = {}
SCANNED_AT = 0.0
PORT = 8766


def source_id(path: Path) -> str:
    return hashlib.sha1(path.as_posix().encode("utf-8")).hexdigest()[:12]


# ---------- read-only SQLite ----------

def _has_live_wal(path: Path) -> bool:
    try:
        return Path(str(path) + "-wal").stat().st_size > 0
    except OSError:
        return False


def connect_ro(path: Path) -> sqlite3.Connection:
    """Open read-only. A file with no live WAL opens immutable, which creates
    no -wal or -shm sidecar. With a WAL, mode=ro lets SQLite read it (it may
    update the existing -shm, which holds no store data)."""
    if path.name.endswith(".zst"):
        if ZSTD_PLAIN is None:
            raise sqlite3.Error("archived chat: " + NO_ZSTD)
        conn = sqlite3.connect(":memory:")
        image = bytearray(ZSTD_PLAIN(path.read_bytes()))
        image[18] = image[19] = 1  # an in-memory image cannot be WAL mode
        conn.deserialize(bytes(image))
    else:
        flag = "mode=ro" if _has_live_wal(path) else "mode=ro&immutable=1"
        uri = "file:" + quote(path.as_posix(), safe="/:") + "?" + flag
        conn = sqlite3.connect(uri, uri=True, timeout=5)
    conn.execute("PRAGMA query_only = 1")
    return conn


def table_names(conn: sqlite3.Connection) -> set[str]:
    return {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'")}


def count_rows(conn, tables: set[str], table: str) -> int | None:
    if table not in tables:
        return None
    return conn.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0]


# ---------- discovery ----------

def is_store(path: Path) -> bool:
    return (path / "catalog.db").is_file() and (path / "chats").is_dir()


def find_stores(root: Path) -> list[Path]:
    if is_store(root):
        return [root]
    found: list[Path] = []

    def walk(directory: Path, depth: int) -> None:
        try:
            entries = list(os.scandir(directory))
        except OSError:
            return
        for entry in entries:
            try:
                if entry.is_dir(follow_symlinks=False) and entry.name not in SKIP_DIRS:
                    path = Path(entry.path)
                    if is_store(path):
                        found.append(path)
                    elif depth < MAX_SCAN_DEPTH:
                        walk(path, depth + 1)
            except OSError:
                continue

    walk(root, 0)
    return found


def catalog_rows(store: Path) -> tuple[dict[str, dict], dict[str, str]]:
    """Root sessions from catalog.db plus root_workspace bindings. Best effort."""
    roots: dict[str, dict] = {}
    workspaces: dict[str, str] = {}
    try:
        with contextlib.closing(connect_ro(store / "catalog.db")) as conn:
            for sid, title, state, last in conn.execute(
                "SELECT id, title, storage_state, last_activity FROM sessions WHERE parent_id IS NULL"
            ):
                roots[sid] = {"title": title, "state": state, "last_activity": last}
            for key, value in conn.execute(
                "SELECT key, value FROM metadata WHERE substr(key,1,15) = 'root_workspace:'"
            ):
                workspaces[key[15:]] = value
    except sqlite3.Error:
        pass
    return roots, workspaces


def workspace_text(value: str) -> str:
    try:
        parsed = json.loads(value)
    except ValueError:
        return value
    if isinstance(parsed, str):
        return parsed
    if isinstance(parsed, dict):
        for k in ("path", "workspace", "root"):
            if isinstance(parsed.get(k), str):
                return parsed[k]
    return value


def scan_all(roots: list[Path], files: list[Path]) -> None:
    global SCANNED_AT
    found: dict[str, Source] = {}
    root_meta: list[dict] = []
    for root in roots:
        resolved = root.resolve()
        stores = find_stores(resolved) if resolved.is_dir() else []
        count = 0
        for store in stores:
            count += add_store(found, store)
        root_meta.append({"label": str(resolved), "exists": resolved.exists(), "count": count})
    for file in files:
        resolved = file.resolve()
        if resolved.is_file():
            add_chat_file(found, resolved, resolved.parent, {}, {}, resolved.parent.name)
    with SCAN_LOCK:
        SOURCES.clear()
        SOURCES.update(found)
        ROOTS[:] = root_meta
        SCANNED_AT = time.time()


def add_store(found: dict[str, Source], store: Path) -> int:
    roots, workspaces = catalog_rows(store)
    if store.name == "store" and store.parent.name in ("BubblePaw-Core", "BubblePaw-base"):
        group = "central store"
    else:
        group = store.parent.name + "/" + store.name if store.name == "store" else store.name
    count = 0
    for chat in sorted((store / "chats").iterdir(), key=lambda p: p.name):
        if chat.name.endswith(".db") or chat.name.endswith(".db.zst"):
            if add_chat_file(found, chat, store, roots, workspaces, group):
                count += 1
    return count


def add_chat_file(found, chat: Path, store: Path, roots, workspaces, group: str) -> bool:
    sid = source_id(chat)
    if sid in found:
        return False
    root_id = chat.name.split(".db")[0]
    meta = roots.get(root_id, {})
    replay_dir = store.parent if store.name == "store" else None
    found[sid] = Source(
        id=sid,
        path=chat,
        store=store,
        display=f"{store.name}/chats/{chat.name}",
        group=group,
        root_label=store.name,
        title=meta.get("title") or root_id[:8],
        workspace=workspace_text(workspaces[root_id]) if root_id in workspaces else None,
        storage_state="archived" if chat.name.endswith(".zst") else meta.get("state", "active"),
        replay_dir=replay_dir,
    )
    return True


# ---------- source info ----------

def session_rows(conn, tables: set[str]) -> list[dict]:
    out: list[dict] = []
    if "sessions" not in tables:
        return out
    for sid, raw in conn.execute("SELECT id, data FROM sessions ORDER BY rowid"):
        try:
            data = json.loads(raw)
        except ValueError:
            data = {}
        out.append({
            "id": sid, "title": data.get("title"), "parent": data.get("parent_session_id"),
            "created_at": data.get("created_at"), "strategy": data.get("context_strategy"),
        })
    return out


def replay_files_of(folder: Path | None) -> list[str]:
    if folder is None or not folder.is_dir():
        return []
    present = [n for n in REPLAY_FILES if (folder / n).is_file()]
    return present + sorted(p.name for p in folder.glob("view-*.txt") if p.is_file())


def chat_workspace(conn, tables: set[str], root_id: str) -> str | None:
    """The root's workspace binding from the chat file's own metadata."""
    if "metadata" not in tables:
        return None
    for key in (f"root_workspace:{root_id}", "workspace"):
        row = conn.execute("SELECT value FROM metadata WHERE key = ?", (key,)).fetchone()
        if row:
            return workspace_text(row[0]).removeprefix("\\\\?\\")
    return None


def inspect_source(src: Source) -> dict:
    try:
        st = src.path.stat()
    except OSError as exc:
        return {"error": f"stat failed: {exc.__class__.__name__}"}
    wal = Path(str(src.path) + "-wal")
    wal_stamp = wal.stat().st_mtime_ns if wal.is_file() else 0
    key = (src.path.as_posix(), st.st_mtime_ns, st.st_size, wal_stamp)
    cached = INFO_CACHE.get(key)
    if cached is not None:
        return cached
    info: dict = {"error": None, "size": st.st_size, "mtime": time.strftime("%Y-%m-%d %H:%M", time.localtime(st.st_mtime))}
    try:
        with contextlib.closing(connect_ro(src.path)) as conn:
            tables = table_names(conn)
            info["schema_version"] = conn.execute("PRAGMA user_version").fetchone()[0]
            info["tables"] = sorted(tables)
            info["missing_tables"] = [t for t in EXPECTED_TABLES if t not in tables]
            rows = session_rows(conn, tables)
            info["session_ids"] = [r["id"] for r in rows]
            info["session_info"] = rows
            info["sessions"] = len(rows)
            info["workspace"] = chat_workspace(conn, tables, src.path.name.split(".db")[0])
            info["counts"] = {
                "messages": count_rows(conn, tables, "messages"),
                "episodes": count_rows(conn, tables, "context_episodes"),
                "subject_chapters": count_rows(conn, tables, "context_subject_chapters"),
                "legacy_chapters": count_rows(conn, tables, "context_chapters"),
                "checkpoints": count_rows(conn, tables, "context_checkpoints"),
                "decisions": count_rows(conn, tables, "context_decisions"),
            }
    except sqlite3.Error as exc:
        info["error"] = f"sqlite: {exc}"
        for k, v in (("counts", {}), ("tables", []), ("missing_tables", []),
                     ("schema_version", None), ("sessions", 0), ("session_ids", []),
                     ("session_info", [])):
            info.setdefault(k, v)
    info["replay_files"] = replay_files_of(src.replay_dir)
    info["has_replay"] = bool(info["replay_files"])
    INFO_CACHE[key] = info
    return info


def source_summary(src: Source) -> dict:
    info = inspect_source(src)
    return {
        "id": src.id,
        "name": src.title,
        "display": src.display,
        "group": src.group,
        "root": src.root_label,
        "storage_state": src.storage_state,
        **({"workspace": src.workspace} if src.workspace else {}),
        **info,
    }


def load_json_text(text, label: str, warnings: list[str]):
    try:
        return json.loads(text)
    except (TypeError, ValueError) as exc:
        warnings.append(f"{label}: invalid JSON ({exc.__class__.__name__})")
        return None


# ---------- decoding stored rows ----------

def blob_text(conn, digest: str, warnings: list[str], label: str) -> str | None:
    """Content of a zstd blob by hash, or None with a warning."""
    row = conn.execute("SELECT raw_bytes, data FROM blobs WHERE hash = ?", (digest,)).fetchone()
    if row is None:
        warnings.append(f"{label}: blob {digest[:8]} is missing")
        return None
    raw_bytes, data = row
    if ZSTD_PLAIN is None:
        return f"[content in zstd blob {digest[:8]} ({raw_bytes} bytes); {NO_ZSTD}]"
    if raw_bytes > MAX_BLOB_BYTES:
        return f"[blob {digest[:8]} is {raw_bytes} bytes, too large to show]"
    try:
        return ZSTD_PLAIN(bytes(data)).decode("utf-8", errors="replace")
    except Exception as exc:  # noqa: BLE001 - a corrupt blob must not break the page
        warnings.append(f"{label}: blob {digest[:8]} unreadable ({exc.__class__.__name__})")
        return None


def decode_body(conn, raw: str, label: str, warnings: list[str]) -> dict | None:
    """The message JSON, undoing the cold-row dictionary codec when present."""
    body = load_json_text(raw, label, warnings)
    if not isinstance(body, dict):
        return None
    if "_row_codec" not in body:
        return body
    if body.get("_row_codec") != "zstd-dict-v1":
        warnings.append(f"{label}: unknown row codec {body.get('_row_codec')}")
        return None
    if ZSTD_DICT is None:
        warnings.append(f"{label}: cold row; {NO_ZSTD}")
        return {"role": "unparsed", "content": f"[cold row, {body.get('raw_bytes')} bytes; {NO_ZSTD}]", "parse_error": True}
    try:
        row = conn.execute("SELECT data FROM message_dictionaries WHERE hash = ?", (body["dictionary"],)).fetchone()
        plain = ZSTD_DICT(base64.b64decode(body["data"]), bytes(row[0]))
        return json.loads(plain)
    except Exception as exc:  # noqa: BLE001
        warnings.append(f"{label}: cold row unreadable ({exc.__class__.__name__})")
        return None


def hydrate_message(conn, body: dict, label: str, warnings: list[str]) -> dict:
    msg = dict(body)
    blob = msg.get("_content_blob")
    if isinstance(blob, str):
        text = blob_text(conn, blob, warnings, label)
        if text is not None:
            msg["content"] = text
    if "_content_parts_blob" in msg or "_content_parts" in msg:
        msg["content_parts_note"] = "content is stored as attachment parts and is not rendered here"
    return msg


def message_by_seq(conn, session_id: str, seq, warnings: list[str]):
    row = conn.execute("SELECT data FROM messages WHERE session_id = ? AND seq = ?", (session_id, int(seq))).fetchone()
    if row is None:
        return None
    body = decode_body(conn, row[0], f"message seq {seq}", warnings)
    return hydrate_message(conn, body, f"message seq {seq}", warnings) if body else None


def checkpoint_data(conn, raw: str, label: str, warnings: list[str]):
    data = load_json_text(raw, label, warnings)
    if isinstance(data, dict) and isinstance(data.get("_summary_blob"), str):
        text = blob_text(conn, data["_summary_blob"], warnings, label)
        if text is not None:
            data["summary"] = text
    return data


# ---------- serving view ----------

def serving_view_row(store: Path, session_id: str, warnings: list[str]):
    path = store / "serving.db"
    if not path.is_file():
        return None
    try:
        with contextlib.closing(connect_ro(path)) as conn:
            if "context_views" not in table_names(conn):
                return None
            row = conn.execute("SELECT data FROM context_views WHERE session_id = ?", (session_id,)).fetchone()
    except sqlite3.Error as exc:
        warnings.append(f"serving.db: {exc}")
        return None
    return load_json_text(row[0], "context view", warnings) if row else None


def source_text(conn, session_id: str, pointer: dict, warnings: list[str]) -> str:
    """The text a view pointer slices, mirroring the engine's byte offsets."""
    table, ident, fld = pointer.get("table"), str(pointer.get("id")), pointer.get("field", "")
    text = ""
    if table == "messages":
        msg = message_by_seq(conn, session_id, ident, warnings) or {}
        text = json.dumps(msg.get(fld[5:]), ensure_ascii=False, separators=(",", ":")) if fld.startswith("json:") else str(msg.get("content", ""))
    elif table == "context_checkpoint_versions":
        row = conn.execute("SELECT summary_hash FROM context_checkpoint_versions WHERE session_id = ? AND id = ?", (session_id, ident)).fetchone()
        text = (blob_text(conn, row[0], warnings, "checkpoint summary") if row else None) or ""
    elif table in ("context_chapters", "context_episodes", "context_subject_chapters"):
        row = conn.execute(f'SELECT data FROM "{table}" WHERE session_id = ? AND id = ?', (session_id, ident)).fetchone()
        data = load_json_text(row[0], f"{table} {ident}", warnings) if row else None
        text = str((data or {}).get(fld) or "")
    else:
        warnings.append(f"view pointer into {table} is not shown")
        return ""
    raw = text.encode("utf-8")
    return raw[pointer.get("start", 0):pointer.get("end", len(raw))].decode("utf-8", errors="replace")


def render_pointers(conn, session_id: str, pointers: list, warnings: list[str]) -> str:
    out = []
    for p in pointers or []:
        if not isinstance(p, dict):
            continue
        out.append(p.get("text", "") if p.get("kind") == "literal" else source_text(conn, session_id, p, warnings))
    return "".join(out)


def expand_view(conn, view: dict, session_id: str, warnings: list[str]) -> dict:
    """Turn the stored prefix_refs into a prefix of messages, as the engine does."""
    refs = view.get("prefix_refs")
    if refs is None:
        return view
    prefix: list[dict] = []
    for ref in refs:
        if ref.get("kind") == "messages":
            for (seq,) in conn.execute(
                "SELECT seq FROM messages WHERE session_id = ? AND seq BETWEEN ? AND ? ORDER BY seq",
                (session_id, ref["first"], ref["last"]),
            ):
                msg = message_by_seq(conn, session_id, seq, warnings)
                if msg:
                    msg["seq"] = seq
                    prefix.append(msg)
        elif ref.get("kind") == "rendered":
            msg = dict(ref.get("metadata") or {})
            msg["content"] = render_pointers(conn, session_id, ref.get("content"), warnings)
            base = message_by_seq(conn, session_id, ref["base_seq"], warnings) if ref.get("base_seq") is not None else None
            for name in ("tool_calls", "provider_state"):
                pointers = (ref.get("fields") or {}).get(name)
                if pointers is not None:
                    try:
                        msg[name] = json.loads(render_pointers(conn, session_id, pointers, warnings))
                    except ValueError:
                        warnings.append(f"view {name} is not valid JSON")
                elif base and name in base:
                    msg[name] = base[name]
            msg["generated"] = True
            if ref.get("base_seq") is not None:
                msg["seq"] = ref["base_seq"]
            prefix.append(msg)
    out = {k: v for k, v in view.items() if k != "prefix_refs"}
    out["prefix"] = prefix
    return out


# ---------- session detail ----------

def session_detail(src: Source, session_id: str) -> dict:
    warnings: list[str] = []
    with contextlib.closing(connect_ro(src.path)) as conn:
        tables = table_names(conn)
        sessions = [r["id"] for r in session_rows(conn, tables)]
        if session_id not in sessions:
            raise KeyError(session_id)
        out: dict = {
            "source": source_summary(src),
            "session": session_id,
            "sessions": sessions,
            "session_info": session_rows(conn, tables),
            "tables": {
                "present": sorted(tables),
                "missing": [t for t in EXPECTED_TABLES if t not in tables],
            },
            "messages": [],
            "episodes": [],
            "chapters": [],
            "legacy_chapters": [],
            "checkpoints": [],
            "decisions": [],
            "view": None,
            "warnings": warnings,
        }
        if "messages" in tables:
            for seq, raw in conn.execute(
                "SELECT seq, data FROM messages WHERE session_id = ? ORDER BY seq", (session_id,)
            ):
                body = decode_body(conn, raw, f"message seq {seq}", warnings)
                if body is None:
                    msg = {"role": "unparsed", "content": raw, "parse_error": True}
                else:
                    msg = hydrate_message(conn, body, f"message seq {seq}", warnings)
                msg["seq"] = seq
                out["messages"].append(msg)
        if "context_episodes" in tables:
            for eid, ordinal, first_seq, last_seq, chapter_id, raw in conn.execute(
                "SELECT id, ordinal, first_message_seq, last_message_seq, chapter_id, data "
                "FROM context_episodes WHERE session_id = ? ORDER BY ordinal",
                (session_id,),
            ):
                out["episodes"].append({
                    "id": eid, "ordinal": ordinal, "first_seq": first_seq, "last_seq": last_seq,
                    "chapter_id": chapter_id,
                    "data": load_json_text(raw, f"episode {ordinal}", warnings),
                })
        if "context_subject_chapters" in tables:
            for cid, ordinal, first_ep, last_ep, status, raw in conn.execute(
                "SELECT id, ordinal, first_episode_ordinal, last_episode_ordinal, status, data "
                "FROM context_subject_chapters WHERE session_id = ? ORDER BY ordinal",
                (session_id,),
            ):
                out["chapters"].append({
                    "id": cid, "ordinal": ordinal, "first_episode_ordinal": first_ep,
                    "last_episode_ordinal": last_ep, "status": status,
                    "data": load_json_text(raw, f"chapter {ordinal}", warnings),
                })
        if "context_chapters" in tables:
            for cid, ordinal, first_seq, last_seq, raw in conn.execute(
                "SELECT id, ordinal, first_message_seq, last_message_seq, data "
                "FROM context_chapters WHERE session_id = ? ORDER BY ordinal",
                (session_id,),
            ):
                out["legacy_chapters"].append({
                    "id": cid, "ordinal": ordinal, "first_seq": first_seq, "last_seq": last_seq,
                    "data": load_json_text(raw, f"legacy chapter {ordinal}", warnings),
                })
        if "context_checkpoints" in tables:
            for cid, through, raw in conn.execute(
                "SELECT id, through_message_seq, data FROM context_checkpoints WHERE session_id = ?",
                (session_id,),
            ):
                out["checkpoints"].append({
                    "id": cid, "through_seq": through,
                    "data": checkpoint_data(conn, raw, "checkpoint", warnings),
                })
        if "context_decisions" in tables:
            for did, ordinal, kind, raw in conn.execute(
                "SELECT id, episode_ordinal, kind, data FROM context_decisions "
                "WHERE session_id = ? ORDER BY episode_ordinal, id",
                (session_id,),
            ):
                out["decisions"].append({
                    "id": did, "episode_ordinal": ordinal, "kind": kind,
                    "data": load_json_text(raw, f"decision {did}", warnings),
                })
        view = serving_view_row(src.store, session_id, warnings)
        if isinstance(view, dict):
            out["view"] = expand_view(conn, view, session_id, warnings)
    return out


def read_text(path: Path, warnings: list[str]) -> str | None:
    try:
        size = path.stat().st_size
        with path.open("rb") as handle:
            raw = handle.read(MAX_TEXT_BYTES)
        text = raw.decode("utf-8", errors="replace")
        if size > MAX_TEXT_BYTES:
            warnings.append(f"{path.name}: truncated at {MAX_TEXT_BYTES} bytes (file is {size})")
        return text
    except OSError as exc:
        warnings.append(f"{path.name}: unreadable ({exc.__class__.__name__})")
        return None


def aggregate_calls(calls: list[dict]) -> list[dict]:
    groups: dict[tuple, dict] = {}
    for call in calls:
        key = (str(call.get("kind")), str(call.get("model")))
        g = groups.setdefault(key, {
            "kind": key[0], "model": key[1], "calls": 0, "input_tokens": 0,
            "output_tokens": 0, "reasoning_tokens": 0, "cost_usd": 0.0, "cost_known": True,
        })
        g["calls"] += 1
        for name in ("input_tokens", "output_tokens", "reasoning_tokens"):
            value = call.get(name)
            if isinstance(value, (int, float)):
                g[name] += int(value)
        cost = call.get("cost_usd")
        if isinstance(cost, (int, float)):
            g["cost_usd"] += float(cost)
        else:
            g["cost_known"] = False
    rows = sorted(groups.values(), key=lambda g: (g["kind"], g["model"]))
    for g in rows:
        g["cost_usd"] = round(g["cost_usd"], 8)
    return rows


def probe_row(probe: dict) -> dict:
    judge = probe.get("judge") if isinstance(probe.get("judge"), dict) else {}
    judgment = judge.get("judgment") if isinstance(judge.get("judgment"), dict) else {}
    probe_usage = probe.get("probe_usage") if isinstance(probe.get("probe_usage"), dict) else {}
    judge_usage = probe.get("judge_usage") if isinstance(probe.get("judge_usage"), dict) else {}
    tool_calls = probe.get("tool_calls") if isinstance(probe.get("tool_calls"), list) else []
    return {
        "id": probe.get("id"),
        "type": probe.get("type"),
        "turn": probe.get("turn"),
        "status": probe.get("status"),
        "question": probe.get("question"),
        "expected_answer": probe.get("expected_answer"),
        "answer": probe.get("answer"),
        "evidence_message_ids": probe.get("evidence_message_ids") or [],
        "frontier": probe.get("frontier"),
        "frontier_message_id": probe.get("frontier_message_id"),
        "end_message_id": probe.get("end_message_id"),
        "compaction_count": probe.get("compaction_count"),
        "grade": judgment.get("grade"),
        "grade_reason": judgment.get("reason"),
        "judge_finish_reason": judge.get("finish_reason"),
        "tool_call_count": len(tool_calls),
        "probe_cost_usd": probe_usage.get("cost_usd"),
        "probe_input_tokens": probe_usage.get("input_tokens"),
        "probe_output_tokens": probe_usage.get("output_tokens"),
        "judge_cost_usd": judge_usage.get("cost_usd"),
    }


def read_json_file(path: Path, warnings: list[str]):
    # Whole file on purpose: report.json can be tens of MB and must parse.
    try:
        raw = path.read_bytes()
    except OSError as exc:
        warnings.append(f"{path.name}: unreadable ({exc.__class__.__name__})")
        return None
    return load_json_text(raw.decode("utf-8", errors="replace"), path.name, warnings)


def replay_key(folder: Path) -> tuple:
    parts = []
    for name in list(REPLAY_FILES) + sorted(p.name for p in folder.glob("view-*.txt")):
        p = folder / name
        try:
            st = p.stat()
        except OSError:
            continue
        parts.append((name, st.st_mtime_ns, st.st_size))
    return (folder.as_posix(), tuple(parts))


REPLAY_CACHE: dict[tuple, dict] = {}


def replay_detail(src: Source) -> dict:
    if src.replay_dir is None:
        return {"source": src.id, "files": [], "report": None, "probes": None, "calls": None, "views": [], "warnings": []}
    key = replay_key(src.replay_dir)
    cached = REPLAY_CACHE.get(key)
    if cached is not None:
        return cached
    out = build_replay_detail(src)
    REPLAY_CACHE.clear()  # keep only the newest folder's output in memory
    REPLAY_CACHE[key] = out
    return out


def build_replay_detail(src: Source) -> dict:
    warnings: list[str] = []
    folder = src.replay_dir
    out: dict = {
        "source": src.id,
        "files": [],
        "report": None,
        "probes": None,
        "calls": None,
        "views": [],
        "warnings": warnings,
    }
    present = [n for n in REPLAY_FILES if (folder / n).is_file()]
    out["files"] = present + sorted(p.name for p in folder.glob("view-*.txt") if p.is_file())

    report = None
    if "report.json" in present:
        report = read_json_file(folder / "report.json", warnings)
    if isinstance(report, dict):
        out["report"] = {k: report.get(k) for k in REPORT_KEYS if k in report}
        out["report"]["probe_count"] = len(report.get("probes") or []) if isinstance(report.get("probes"), list) else None
        out["report"]["episode_count"] = len(report.get("episodes") or []) if isinstance(report.get("episodes"), list) else None
        out["report"]["decision_count"] = len(report.get("decisions") or []) if isinstance(report.get("decisions"), list) else None
        out["report"]["failure_count"] = len(report.get("failures") or []) if isinstance(report.get("failures"), list) else None

    if "probes.json" in present:
        probes = read_json_file(folder / "probes.json", warnings)
        if isinstance(probes, list):
            out["probes"] = [probe_row(p) for p in probes if isinstance(p, dict)]

    calls: list[dict] = []
    if "calls.jsonl" in present:
        text = read_text(folder / "calls.jsonl", warnings) or ""
        for number, line in enumerate(text.splitlines(), 1):
            if not line.strip():
                continue
            parsed = load_json_text(line, f"calls.jsonl line {number}", warnings)
            if isinstance(parsed, dict):
                calls.append(parsed)
    elif isinstance(report, dict) and isinstance(report.get("calls"), list):
        calls = [c for c in report["calls"] if isinstance(c, dict)]
    if calls or "calls.jsonl" in present:
        out["calls"] = {"rows": aggregate_calls(calls), "total_calls": len(calls)}

    # Names and sizes only; texts are fetched one at a time from /views/<name>.
    views = []
    for name in out["files"]:
        if VIEW_NAME_RE.match(name) and (folder / name).is_file():
            views.append({"name": name, "bytes": (folder / name).stat().st_size})
    out["views"] = views
    return out


class Handler(BaseHTTPRequestHandler):
    server_version = "lcm-viewer/0"

    def log_message(self, fmt, *args):  # quiet by default; errors still go to stderr
        if os.environ.get("LCM_VIEWER_LOG"):
            sys.stderr.write("%s %s\n" % (self.address_string(), fmt % args))

    def _allowed_host(self) -> bool:
        host = (self.headers.get("Host") or "").strip().lower()
        return host in {f"127.0.0.1:{PORT}", f"localhost:{PORT}"}

    def _send(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.end_headers()
        self.wfile.write(body)

    def _json(self, status: int, payload) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self._send(status, body, "application/json; charset=utf-8")

    def _error(self, status: int, message: str) -> None:
        self._json(status, {"error": message})

    def do_GET(self) -> None:  # noqa: N802
        if not self._allowed_host():
            self._error(403, "host not allowed")
            return
        parts = urlsplit(self.path)
        segments = [unquote(s) for s in parts.path.split("/") if s]
        try:
            if not segments or segments == ["index.html"]:
                self._send(200, INDEX_HTML.read_bytes(), "text/html; charset=utf-8")
                return
            if segments == ["api", "sources"]:
                if "rescan=1" in (parts.query or ""):
                    scan_all(ROOT_PATHS, FILE_PATHS)
                with SCAN_LOCK:
                    items = list(SOURCES.values())
                payload = {
                    "roots": ROOTS,
                    "scanned_at": time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(SCANNED_AT)),
                    "sources": [source_summary(s) for s in sorted(items, key=lambda s: (s.group, s.path.name, s.display))],
                }
                self._json(200, payload)
                return
            if len(segments) >= 3 and segments[0] == "api" and segments[1] == "sources":
                src = SOURCES.get(segments[2]) if ID_RE.match(segments[2]) else None
                if src is None:
                    self._error(404, "unknown source")
                    return
                if len(segments) == 3:
                    self._json(200, source_summary(src))
                    return
                if segments[3:] == ["replay"]:
                    self._json(200, replay_detail(src))
                    return
                if len(segments) == 5 and segments[3] == "views":
                    name = segments[4]
                    path = (src.replay_dir or src.path.parent) / name
                    if not VIEW_NAME_RE.match(name) or not path.is_file():
                        self._error(404, "unknown view")
                        return
                    with path.open("rb") as handle:
                        body = handle.read(MAX_TEXT_BYTES)
                    self._send(200, body, "text/plain; charset=utf-8")
                    return
                if len(segments) == 5 and segments[3] == "sessions":
                    self._json(200, session_detail(src, segments[4]))
                    return
            self._error(404, "not found")
        except KeyError:
            self._error(404, "unknown session")
        except sqlite3.Error as exc:
            self._error(500, f"sqlite error: {exc}")
        except OSError as exc:
            self._error(500, f"io error: {exc.__class__.__name__}")

    def do_POST(self) -> None:  # noqa: N802
        self._error(405, "read-only viewer")

    do_PUT = do_DELETE = do_PATCH = do_POST


ROOT_PATHS: list[Path] = []
FILE_PATHS: list[Path] = []


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Read-only BubblePaw store and LCM viewer on 127.0.0.1.")
    parser.add_argument("--port", type=int, default=8766)
    parser.add_argument("--data-dir", "--root", dest="data_dirs", action="append", default=[], metavar="DIR",
                        help="store directory (catalog.db and chats/), or a folder holding stores such as "
                             "replay outputs (repeatable; default: the machine's central store)")
    parser.add_argument("files", nargs="*", metavar="CHAT.db", help="individual chat files to add")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    global PORT, ROOT_PATHS, FILE_PATHS
    args = parse_args(sys.argv[1:] if argv is None else argv)
    PORT = args.port
    roots = [Path(r) for r in args.data_dirs] or [default_data_dir()]
    seen: set[str] = set()
    ROOT_PATHS = []
    for r in roots:
        key = str(r.resolve())
        if key not in seen:
            seen.add(key)
            ROOT_PATHS.append(r)
    FILE_PATHS = [Path(f) for f in args.files]
    for f in FILE_PATHS:
        if not f.is_file():
            print(f"error: not a file: {f}", file=sys.stderr)
            return 2
    scan_all(ROOT_PATHS, FILE_PATHS)
    for meta in ROOTS:
        if not meta["exists"]:
            print(f"warning: {meta['label']} does not exist", file=sys.stderr)
        elif not meta["count"]:
            print(f"warning: no store (catalog.db and chats/) found under {meta['label']}", file=sys.stderr)
    if ZSTD_PLAIN is None:
        print(f"note: {NO_ZSTD}; compressed message bodies and summaries show as placeholders", file=sys.stderr)
    try:
        server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    except OSError as exc:
        print(f"error: cannot bind 127.0.0.1:{PORT}: {exc}", file=sys.stderr)
        return 1
    print(f"LCM viewer on http://127.0.0.1:{PORT}/  ({len(SOURCES)} chats)", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
