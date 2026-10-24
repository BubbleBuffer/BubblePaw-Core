// Session mirror: one Claude Code session per BubblePaw session, kept in sync
// from the harness's history cursor.
//
// Adapted from pi-claude-bridge src/index.ts (SessionState, syncSharedSession's
// REUSE and REBUILD paths, convertAndImportMessages, verifyWrittenSession).
// pi-claude-bridge is MIT, Copyright (c) 2026 Eli Dickinson; see NOTICE.
//
// Changes from pcb: pcb infers history changes by counting messages against a
// cursor. BubblePaw's harness tells us instead (docs/providers.md): the same
// `history.epoch` means the request only appended, a new epoch means rebuild.
// The count is still kept, to find where the appended part starts, and the
// assistant message our last step produced is checked through its
// `provider_state` before anything is reused.
//
// TODO(docs/providers.md, "Claude Code provider", planned improvements): native
// records in provider_state written back verbatim, tail-only rebuilds from
// `rewritten_from`, adopting records Claude Code wrote that the store lacks.
// Today every rebuild rewrites the whole session file.

import { createSession, deleteSession, repairToolPairing } from "cc-session-io";
import { readFileSync } from "node:fs";
import { convertMessages, promptText, systemAsUserText, turnStart } from "./convert.js";
import { replayCompatible } from "./replay-state.js";
import type { Message, ModelRequest } from "./types.js";

export interface Mirror {
	/** The model whose native session can be resumed; absent in legacy mirrors. */
	model?: string;
	/** Claude Code's session id; its JSONL lives under the Claude config dir. */
	ccSessionId: string;
	/** The harness epoch this mirror is in sync with. */
	epoch: string;
	/** How many stable (non-ephemeral) request messages the Claude Code session
	 *  holds, not counting the assistant message the last step returned (expected
	 *  at the stable message list's `[cursor]`). */
	cursor: number;
	/** What the last step returned, to recognise it in the next request. */
	lastStep: { toolUseIds: string[] };
	/** Force the next sync down REBUILD (after an abort or a failed query). */
	needsRebuild: boolean;
	/** Set only where we just killed a Claude Code process: it may still flush a
	 *  late record into the session file, so the rebuild takes a fresh id instead
	 *  of rewriting the same file (pcb's forceRotate). */
	forceRotate: boolean;
	cwd: string;
}

/** What the extension knows about a running Claude Code query for this session. */
export interface LiveQuery {
	alive: boolean;
	/** The query's prompt stream still accepts messages (for steering). */
	steerable?: boolean;
	model: string;
	toolNames: string[];
}

/** Prefix of a message id that marks per-step context (the harness's runtime
 *  note), not durable history (docs/providers.md, Interface). The cursor ignores
 *  such messages; so does the mirror: they are delivered as `<system-note>` text
 *  with the step that carries them and never counted. */
export const EPHEMERAL_ID_PREFIX = "ephemeral:";

export function isEphemeral(message: Message): boolean {
	return typeof message.id === "string" && message.id.startsWith(EPHEMERAL_ID_PREFIX);
}

/** Ephemeral messages as one `<system-note>` text, or null when there are none. */
export function notesText(messages: readonly Message[]): string | null {
	const text = messages.filter((m) => m.content).map((m) => systemAsUserText(m.content)).join("\n\n");
	return text || null;
}

export type Decision =
	/** Release the parked calls with `results`; `steer` are user messages that
	 *  arrived with them, pushed into the running query; `notes` the request's
	 *  ephemeral messages, attached to the delivery. */
	| { kind: "deliver"; results: Message[]; steer: Message[]; notes: Message[] }
	| { kind: "reuse"; prompt: string }
	| { kind: "rebuild"; priors: Message[]; prompt: string; reason: string };

/** Prompt for a rebuilt session whose history ends at a tool result rather than
 *  a prompt (pcb's CONTINUE_AFTER_REWRITE_PROMPT): query() cannot resume a
 *  session without one. */
export const CONTINUE_PROMPT =
	"[Your context was rebuilt. What precedes this is the conversation so far, "
	+ "ending with the tool result you were waiting for. Continue the task from there.]";

/** The `provider_state` our step results carry. */
export interface ClaudeCodeState {
	/** `steered`: ids of the user messages that step pushed into the running query. */
	claude_code: { session_id: string; tool_use_ids: string[]; message_id?: string; steered?: string[] };
}

export function claudeCodeState(value: unknown): ClaudeCodeState["claude_code"] | undefined {
	const state = (value as ClaudeCodeState | undefined)?.claude_code;
	return state && typeof state.session_id === "string" ? state : undefined;
}

function sameSet(a: readonly string[], b: readonly string[]): boolean {
	if (a.length !== b.length) return false;
	const set = new Set(a);
	return b.every((x) => set.has(x));
}

/** The number of stable messages, which is what the mirror's cursor counts. */
export function stableCount(messages: readonly Message[]): number {
	return messages.filter((m) => !isEphemeral(m)).length;
}

/** A rebuild writes the request as it is: its current ephemeral messages stay
 *  at their position (the request holds no earlier ones). A trailing turn made
 *  only of ephemeral messages is not a prompt; the continue prompt follows it. */
function rebuild(request: ModelRequest, reason: string): Decision {
	const messages = request.messages;
	const start = turnStart(messages);
	const priors = messages.slice(0, start);
	const turn = messages.slice(start);
	let prompt: string | null;
	if (turn.some((m) => !isEphemeral(m))) {
		prompt = promptText(messages);
	} else {
		const last = priors[priors.length - 1];
		const fallback = last?.role === "tool" ? CONTINUE_PROMPT : "[continue]";
		prompt = [notesText(turn), fallback].filter((t) => t).join("\n\n");
	}
	return { kind: "rebuild", priors, prompt: prompt ?? "[continue]", reason };
}

/** Reuse versus rebuild, from the cursor (docs/providers.md):
 *  - same epoch and the request only appends after our last step: reuse, either
 *    by releasing the parked tool calls (`deliver`) or by resuming the session
 *    with the new prompt (`reuse`);
 *  - a new epoch, or anything inconsistent: rebuild from the request.
 *  Ephemeral messages take no part in the comparison or the count. */
export function decide(mirror: Mirror | undefined, request: ModelRequest, live: LiveQuery | undefined): Decision {
	if (!mirror) return rebuild(request, "no mirror");
	if (mirror.model && mirror.model !== request.model) return rebuild(request, "model changed");
	if (mirror.needsRebuild) return rebuild(request, "needs rebuild");
	const epoch = request.history?.epoch;
	if (!epoch || epoch !== mirror.epoch) return rebuild(request, "new epoch");
	const messages = request.messages.filter((m) => !isEphemeral(m));
	const notes = request.messages.filter(isEphemeral);
	const ours = messages[mirror.cursor];
	if (!ours || ours.role !== "assistant") return rebuild(request, "last step's reply missing");
	const state = claudeCodeState(ours.provider_state);
	if ((ours.provider_state as any)?._bubblepaw_origin && !replayCompatible(ours.provider_state, request.model)) {
		return rebuild(request, "native provider state does not match the model");
	}
	const callIds = (ours.tool_calls ?? []).map((c) => c.id);
	if (!state || state.session_id !== mirror.ccSessionId || !sameSet(callIds, mirror.lastStep.toolUseIds)) {
		return rebuild(request, "last step's reply does not match the mirror");
	}
	const tail = messages.slice(mirror.cursor + 1);

	if (mirror.lastStep.toolUseIds.length > 0) {
		// The tool results, then any user messages steered in while the calls ran
		// (the harness appends steering after a complete tool round). Those reach
		// the running query through its prompt stream; anything else rebuilds.
		let split = 0;
		while (split < tail.length && tail[split].role === "tool") split++;
		const results = tail.slice(0, split);
		const steer = tail.slice(split);
		if (steer.some((m) => m.role !== "user")) return rebuild(request, "messages besides tool results and user messages");
		const resultIds = results.map((m) => m.tool_call_id ?? "");
		if (!sameSet(resultIds, mirror.lastStep.toolUseIds)) return rebuild(request, "tool results do not match the parked calls");
		if (!live?.alive) return rebuild(request, "parked query is gone");
		if (live.model !== request.model) return rebuild(request, "model changed");
		if (!sameSet(live.toolNames, request.tools.map((t) => t.name))) return rebuild(request, "tools changed");
		if (steer.length > 0 && !live.steerable) return rebuild(request, "steering needs a prompt stream");
		return { kind: "deliver", results, steer, notes };
	}

	if (tail.length === 0 || tail.some((m) => m.role !== "user" && m.role !== "system")) {
		return rebuild(request, "no new prompt after the last reply");
	}
	// Ephemeral context goes before the next user text.
	const prompt = [notesText(notes), promptText(tail)].filter((t) => t).join("\n\n");
	return { kind: "reuse", prompt: prompt || "[continue]" };
}

export interface WriteResult {
	sessionId: string | null;
	records: number;
	warnings: string[];
}

/** pcb's REBUILD: write a Claude Code session JSONL holding `priors`. Rewrites the
 *  previous session in place (same id) unless `forceRotate`, which takes a fresh
 *  id so a killed process's late writes land on the old file. */
export function writeSession(opts: {
	priors: readonly Message[];
	cwd: string;
	claudeDir?: string;
	previousId?: string;
	forceRotate?: boolean;
	model?: string;
}): WriteResult {
	if (opts.priors.length === 0) return { sessionId: null, records: 0, warnings: [] };
	const preserveId = opts.previousId !== undefined && !opts.forceRotate;
	if (preserveId) deleteSession(opts.previousId!, opts.cwd, opts.claudeDir);
	const session = createSession({
		projectPath: opts.cwd,
		claudeDir: opts.claudeDir,
		...(preserveId ? { sessionId: opts.previousId } : {}),
		...(opts.model ? { model: opts.model } : {}),
	});
	const { messages } = convertMessages(opts.priors, opts.model);
	const repaired = repairToolPairing(messages);
	if (repaired.length) session.importMessages(repaired);
	session.save();
	const warnings = verifyWrittenSession(session.jsonlPath, session.sessionId, session.records.length);
	return { sessionId: session.sessionId, records: session.records.length, warnings };
}

/** pcb src/session-verify.ts. */
export function verifyWrittenSession(jsonlPath: string, expectedSessionId: string, expectedRecordCount: number): string[] {
	let content: string;
	try {
		content = readFileSync(jsonlPath, "utf8");
	} catch (e) {
		return [`file missing after save: ${jsonlPath}: ${(e as Error).message}`];
	}
	const lines = content.split("\n").filter((l) => l.trim().length > 0);
	if (lines.length !== expectedRecordCount) {
		return [`record count mismatch: expected ${expectedRecordCount}, found ${lines.length} in ${jsonlPath}`];
	}
	try {
		const first = JSON.parse(lines[0]);
		const last = JSON.parse(lines[lines.length - 1]);
		if (first.sessionId !== expectedSessionId || last.sessionId !== expectedSessionId) {
			return [`sessionId drift: expected ${expectedSessionId}, first ${first.sessionId}, last ${last.sessionId}`];
		}
	} catch (e) {
		return [`malformed JSONL in ${jsonlPath}: ${(e as Error).message}`];
	}
	return [];
}
