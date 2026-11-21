// Reuse versus reseed, from the host's history cursor (docs/providers.md):
// the extension keeps one agy session per BubblePaw session and remembers how far
// into the request's message list it has fed it.
//
//  - same epoch, same model/effort/system prompt/tools, and the request only
//    appends after our last reply: `deliver` (the tool results for the parked
//    calls were appended) or `reuse` (new user input after a finished reply);
//  - anything else (new epoch: summary, rewind, fork, restart; a changed model or
//    tool set; a reply that is not ours): `reseed`, a new agy session seeded from
//    the request.
//
// Ephemeral messages take no part in the comparison or the count.

import { createHash } from "node:crypto";
import { isEphemeral, promptText, systemAsText } from "./seed.js";
import type { Message, ModelRequest } from "./types.js";

export interface Mirror {
	acpSessionId: string;
	epoch: string | undefined;
	/** Hash of model, effort, system prompt and tool definitions: a change reseeds. */
	configKey: string;
	/** Non-ephemeral messages fed so far; the next request has our reply at this index. */
	cursor: number;
	/** BubblePaw tool call ids of the last step (parked in agy), empty after a final reply. */
	lastCallIds: string[];
	/** Whether the agy turn that parked those calls is still running. */
	turnAlive: boolean;
}

export type Decision =
	| { kind: "deliver"; results: Message[]; steer: Message[]; notes: Message[] }
	| { kind: "reuse"; prompt: string }
	| { kind: "reseed"; reason: string };

export function configKey(request: ModelRequest): string {
	return createHash("sha1")
		.update(JSON.stringify([request.model, request.reasoning_effort ?? null, request.system_prompt, request.tools]))
		.digest("hex");
}

export function antigravityState(state: unknown): { session_id: string; call_ids: string[] } | undefined {
	const s = (state as any)?.antigravity;
	return s && typeof s.session_id === "string" && Array.isArray(s.call_ids) ? s : undefined;
}

const sameSet = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((x) => b.includes(x));

export function notesText(notes: readonly Message[]): string {
	return notes.map((m) => systemAsText(m.content)).join("\n\n");
}

export function decide(mirror: Mirror | undefined, request: ModelRequest): Decision {
	const reseed = (reason: string): Decision => ({ kind: "reseed", reason });
	if (!mirror) return reseed("no session");
	const epoch = request.history?.epoch;
	if (!epoch || epoch !== mirror.epoch) return reseed("new epoch");
	if (mirror.configKey !== configKey(request)) return reseed("model, effort, system prompt or tools changed");
	const messages = request.messages.filter((m) => !isEphemeral(m));
	const notes = request.messages.filter(isEphemeral);
	const ours = messages[mirror.cursor];
	if (!ours || ours.role !== "assistant") return reseed("last reply missing");
	const state = antigravityState(ours.provider_state);
	const callIds = (ours.tool_calls ?? []).map((c) => c.id);
	if (!state || state.session_id !== mirror.acpSessionId || !sameSet(callIds, mirror.lastCallIds)) return reseed("last reply is not ours");
	const tail = messages.slice(mirror.cursor + 1);

	if (mirror.lastCallIds.length > 0) {
		let split = 0;
		while (split < tail.length && tail[split].role === "tool") split++;
		const results = tail.slice(0, split);
		const steer = tail.slice(split);
		if (steer.some((m) => m.role !== "user")) return reseed("messages besides tool results and user messages");
		if (!sameSet(results.map((m) => m.tool_call_id ?? ""), mirror.lastCallIds)) return reseed("tool results do not match the parked calls");
		if (!mirror.turnAlive) return reseed("parked turn is gone");
		return { kind: "deliver", results, steer, notes };
	}
	if (tail.length === 0 || tail.some((m) => m.role !== "user" && m.role !== "system")) return reseed("no new input after the last reply");
	const prompt = [notesText(notes), promptText(tail)].filter((t) => t).join("\n\n");
	return { kind: "reuse", prompt: prompt || "[continue]" };
}
