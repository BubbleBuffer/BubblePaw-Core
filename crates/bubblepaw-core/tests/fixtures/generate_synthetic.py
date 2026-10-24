"""Write deterministic, wholly synthetic native transcripts for phase C.

The large payload is generated in memory one turn at a time. Nothing is copied
from the private transcript fixtures. Run with --turns 10000 --payload-bytes 8192
for a repeatable larger chat, or use the small default for shape inspection.
"""
import argparse
import json
from pathlib import Path

STAMP = "2026-10-12T00:00:00Z"


def write(path, events):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as stream:
        for event in events:
            stream.write(json.dumps(event, separators=(",", ":")) + "\n")


def codex(turns, payload_bytes, parent=None):
    source = {"subagent": {"thread_spawn": {"parent_thread_id": parent}}} if parent else "cli"
    yield {"type": "session_meta", "payload": {"id": "synthetic-agent" if parent else "synthetic-root", "source": source, "model": "synthetic-gpt-a", "cli_version": "synthetic-v2"}}
    for index in range(turns):
        model = "synthetic-gpt-a" if index < turns // 2 else "synthetic-gpt-b"
        yield {"type": "turn_context", "payload": {"model": model}}
        prompt = f"Synthetic question {index}."
        yield {"type": "response_item", "timestamp": STAMP,
               "payload": {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Injected harness instructions."}]}}
        yield {"type": "response_item", "timestamp": STAMP,
               "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": f"<environment_context>ambient</environment_context>{prompt}"}]}}
        yield {"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "UserMessage", "content": [{"type": "text", "text": prompt}]}}}
        for kind, field in [("function_call", "arguments"), ("custom_tool_call", "input")]:
            call = f"{kind}-{index}"
            yield {"type": "response_item", "timestamp": STAMP, "payload": {"type": kind, "call_id": call, "name": "synthetic_tool", field: '{"value":1}' if field == "arguments" else "raw custom input"}}
            yield {"type": "response_item", "timestamp": STAMP, "payload": {"type": kind + "_output", "call_id": call, "output": "x" * payload_bytes}}
        yield {"type": "response_item", "timestamp": STAMP, "payload": {
            "type": "reasoning", "id": f"reason-{index}",
            "summary": [{"type": "summary_text", "text": "Synthetic plaintext reasoning."}],
            "encrypted_content": f"SYNTHETIC_CIPHERTEXT_PRESERVED:{index}+/=\\\n雪" + "opaque-bytes " * 512,
            "internal_chat_message_metadata_passthrough": {"exact": "unknown opaque field\t\\雪"},
            "provider_extension": {"signed_zero": -0.0, "keep": [True, None, "opaque"]}}}
        yield {"type": "response_item", "timestamp": STAMP, "payload": {"type": "web_search_call", "id": f"web-{index}", "action": {"type": "search", "query": "synthetic query"}, "status": "completed"}}
        yield {"type": "response_item", "timestamp": STAMP, "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": f"Synthetic answer {index}."}]}}
        if index % 4 == 0:
            yield {"type": "compacted", "timestamp": STAMP, "payload": {"message": f"Source summary through synthetic turn {index}.", "replacement_history": [], "guardian_history": [], "window_number": index}}


def claude(turns, payload_bytes):
    for index in range(turns):
        common = {"timestamp": STAMP, "sessionId": "synthetic-claude", "uuid": f"user-{index}"}
        yield {**common, "type": "user", "promptSource": "sdk", "message": {"role": "user", "content": f"Real synthetic prompt {index}. <system-reminder>This is typed text.</system-reminder>"}}
        yield {**common, "type": "user", "isMeta": True, "message": {"role": "user", "content": "Injected harness material."}}
        model = "synthetic-claude-a" if index < turns // 2 else "synthetic-claude-b"
        yield {**common, "uuid": f"thinking-{index}", "type": "assistant", "message": {"role": "assistant", "model": model, "content": [
            {"type": "thinking", "thinking": "Synthetic signed plaintext.\n雪", "signature": "synthetic-signature-exact+/=\\\n", "provider_extension": {"unknown": "keep exact"}},
            {"type": "redacted_thinking", "data": f"SYNTHETIC_REDACTED_PRESERVED:{index}+/=\\\n雪" + "opaque-bytes " * 512, "provider_extension": {"unknown": "retain redacted extension"}}
        ]}}
        yield {**common, "uuid": f"assistant-{index}", "type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Calling synthetic tool."}, {"type": "tool_use", "id": f"tool-{index}", "name": "synthetic", "input": {"index": index}, "caller": {"type": "direct"}}]}}
        yield {**common, "uuid": f"result-{index}", "type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": f"tool-{index}", "content": [{"type": "text", "text": "y" * payload_bytes}], "is_error": False}]}}
        if index % 4 == 0:
            yield {**common, "type": "system", "subtype": "compact_boundary", "compactMetadata": {"trigger": "auto"}}
            yield {**common, "type": "user", "isCompactSummary": True, "message": {"role": "user", "content": "Source-generated synthetic summary."}}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--turns", type=int, default=8)
    parser.add_argument("--payload-bytes", type=int, default=4096)
    args = parser.parse_args()
    if args.turns < 1 or args.payload_bytes < 0:
        parser.error("turns must be positive and payload bytes nonnegative")
    write(args.out / "codex.jsonl", codex(args.turns, args.payload_bytes))
    write(args.out / "codex-agent.jsonl", codex(1, args.payload_bytes, "synthetic-root"))
    write(args.out / "claude.jsonl", claude(args.turns, args.payload_bytes))
    write(args.out / "claude" / "subagents" / "agent-synthetic.jsonl", claude(1, args.payload_bytes))
    print(json.dumps({"turns": args.turns, "payload_bytes": args.payload_bytes, "files": 4}))
