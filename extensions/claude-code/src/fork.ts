// Summary forks: a side call that resumes a copy of the mirrored Claude Code
// session, cut after a given BubblePaw message, with one extra user message
// (docs/providers.md, "Summary forks"). The parent's prompt cache is reused
// because the fork keeps its system prompt, tools and options, and its
// conversation is a prefix of the parent's.
//
// The mirror and the parent's session file are only read. The SDK's
// `resumeSessionAt` + `forkSession` cut the copy at a record uuid and
// `persistSession: false` keeps the fork's own file from being written, so a
// fork leaves nothing behind. The cut is found by aligning the session file's
// records with the request's messages (`forkCut`); where the two do not line up
// or the file is not complete up to the cut, the fork fails `transient`, and the
// host's gather asks again later.

import { getSessionPath } from "cc-session-io";
import { readFileSync } from "node:fs";
import { sanitizeToolId } from "./convert.js";
import { isEphemeral, type Mirror } from "./mirror.js";
import { ProviderError, type Message, type ModelRequest } from "./types.js";

/** What the fork resumes from: the stable messages up to the cut, and the one
 *  user message (the branch instruction and return control) asked there. */
export function forkParts(request: ModelRequest): { prefix: Message[]; prompt: string } {
	const last = request.messages.at(-1);
	if (last?.role !== "user" || !last.content) throw new ProviderError("fatal", "a summary fork request must end in one user message");
	const prefix = request.messages.slice(0, -1).filter((m) => !isEphemeral(m));
	if (prefix.length === 0) throw new ProviderError("fatal", "a summary fork needs a conversation to branch");
	return { prefix, prompt: last.content };
}

/** Options that make the query a fork which cannot run a tool: the SDK's
 *  `canUseTool` refuses every call (the host executes tools, never this process,
 *  so a refused call is also never parked for release). Measured: neither the
 *  permission mode nor `canUseTool` changes the cached prefix. */
export function forkOptions(parentId: string, cutUuid: string) {
	return {
		resume: parentId,
		forkSession: true,
		resumeSessionAt: cutUuid,
		persistSession: false,
		permissionMode: "default",
		canUseTool: async () => ({ behavior: "deny" as const, message: "No tools in this task; answer in text." }),
	};
}

/** The parent session file's records, without a trailing line still being written. */
export function readRecords(mirror: Mirror, claudeDir?: string): any[] {
	let text: string;
	try {
		text = readFileSync(getSessionPath(mirror.ccSessionId, mirror.cwd, claudeDir), "utf8");
	} catch {
		throw unavailable("the session file cannot be read");
	}
	const records: any[] = [];
	for (const line of text.split("\n")) {
		if (!line.trim()) continue;
		try {
			records.push(JSON.parse(line));
		} catch {}
	}
	return records;
}

/** A cut this session file can never be forked at, as opposed to one not written yet. */
function impossible(reason: string): ProviderError {
	const error = new ProviderError("fatal", `no fork cut possible: ${reason}`);
	error.forkImpossible = true;
	return error;
}

function unavailable(reason: string): ProviderError {
	return new ProviderError("transient", `no consistent fork cut: ${reason}`);
}

/** The session file's conversation in message terms: an assistant message
 *  (its records share one API message id; `text` says one carried text), one
 *  tool result, or one block of user text (`note`: a runtime note, which rides
 *  along with a turn and is no BubblePaw message). `rec` is its record. */
type Item =
	| { k: "a"; uuid: string; mid: string; ids: string[]; text: boolean; rec: number }
	| { k: "t"; uuid: string; id: string; rec: number }
	| { k: "u"; uuid: string; rec: number; note: boolean };

function items(records: readonly any[]): Item[] {
	const out: Item[] = [];
	records.forEach((r, rec) => {
		if ((r?.type !== "user" && r?.type !== "assistant") || r.isSidechain || r.isMeta || !r.uuid) return;
		const content = r.message?.content;
		const blocks: any[] = Array.isArray(content) ? content : typeof content === "string" ? [{ type: "text", text: content }] : [];
		if (r.type === "assistant") {
			const ids = blocks.filter((b) => b.type === "tool_use").map((b) => b.id as string);
			const text = blocks.some((b) => b.type === "text" && b.text);
			const prev = out.at(-1);
			if (prev?.k === "a" && r.message?.id && prev.mid === r.message.id) {
				prev.uuid = r.uuid;
				prev.ids.push(...ids);
				prev.text ||= text;
				prev.rec = rec;
			} else out.push({ k: "a", uuid: r.uuid, mid: r.message?.id ?? "", ids, text, rec });
			return;
		}
		for (const b of blocks) {
			if (b.type === "tool_result") out.push({ k: "t", uuid: r.uuid, id: b.tool_use_id, rec });
			else out.push({ k: "u", uuid: r.uuid, rec, note: typeof b.text === "string" && b.text.startsWith("<system-note>") });
		}
	});
	return out;
}

/** The uuid of the session file's record that ends `prefix`, or an error when
 *  the file does not hold exactly that conversation up to it. Tool-use ids and
 *  the order of assistant messages, tool results and user turns are compared; a
 *  run of user and system messages is one turn however it was written. A cut
 *  inside a record (results and a steering text in one record) or at a user
 *  message (where a run's end cannot be told) is refused. */
export function forkCut(records: readonly any[], prefix: readonly Message[]): string {
	const list = items(records);
	const cache = new Map<string, string>();
	let j = 0;
	let last: Item | undefined;
	const skipNotes = () => {
		while (list[j]?.k === "u" && (list[j] as { note: boolean }).note) j++;
	};
	for (let p = 0; p < prefix.length; p++) {
		const m = prefix[p];
		if (m.role === "user" || m.role === "system") {
			if (list[j]?.k !== "u") throw unavailable("a user turn is missing from the session file");
			while (list[j]?.k === "u") last = list[j++];
			while (prefix[p + 1]?.role === "user" || prefix[p + 1]?.role === "system") p++;
			if (p === prefix.length - 1) throw impossible("the cut is at a user message");
		} else if (m.role === "assistant") {
			if (!m.content && !(m.tool_calls ?? []).length) continue;
			skipNotes();
			const item = list[j];
			const ids = (m.tool_calls ?? []).map((c) => sanitizeToolId(c.id, cache));
			if (item?.k !== "a" || ids.length !== item.ids.length || ids.some((id) => !item.ids.includes(id))) {
				throw unavailable("an assistant message does not match the session file");
			}
			if (m.content && !item.text) throw unavailable("an assistant message is not fully written yet");
			last = item;
			j++;
		} else {
			skipNotes();
			const item = list[j];
			if (item?.k !== "t" || item.id !== sanitizeToolId(m.tool_call_id ?? "", cache)) throw unavailable("a tool result does not match the session file");
			last = item;
			j++;
		}
	}
	if (!last) throw unavailable("the session file holds no conversation");
	const next = list[j];
	if (last.k === "t" && next?.rec === last.rec && !(next.k === "u" && next.note)) throw impossible("the cut falls inside a record");
	return last.uuid;
}
