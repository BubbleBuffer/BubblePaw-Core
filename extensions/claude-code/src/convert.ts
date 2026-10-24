// BubblePaw message history -> Anthropic messages for a Claude Code session
// rebuild (cc-session-io's importMessages), plus the history/prompt split.
//
// Adapted from pi-claude-bridge src/convert.ts (convertPiMessages, sanitizeToolId,
// mapPiToolNameToSdk) and src/index.ts (turnStart, extractUserPrompt).
// pi-claude-bridge is MIT, Copyright (c) 2026 Eli Dickinson; see NOTICE.
//
// Changes from pcb: BubblePaw messages carry plain-string content and separate
// `tool_calls`, tool results are `tool` messages, and `system` messages (summaries,
// runtime notes) have no Anthropic equivalent inside the conversation, so they are
// carried as user text. Compatible native blocks retain their order and opaque
// fields; their ordinary text and calls are hydrated from the canonical message.

import type { Message as SessionMessage } from "cc-session-io";
import type { Message } from "./types.js";
import { replayContent } from "./replay-state.js";

export const MCP_SERVER_NAME = "bubblepaw";
export const MCP_TOOL_PREFIX = `mcp__${MCP_SERVER_NAME}__`;

/** The name Claude Code sees for a served tool. */
export function sdkToolName(name: string): string {
	return `${MCP_TOOL_PREFIX}${name}`;
}

/** A served tool name back to the BubblePaw name, or undefined for anything we do
 *  not serve (pcb's piToolNameFor: with `tools: []` any other name is the model
 *  hallucinating a builtin, which Claude Code rejects and retries itself). */
export function bubblepawToolName(sdkName: string, served: ReadonlySet<string>): string | undefined {
	if (!sdkName.startsWith(MCP_TOOL_PREFIX)) return undefined;
	const name = sdkName.slice(MCP_TOOL_PREFIX.length);
	return served.has(name) ? name : undefined;
}

/** Anthropic tool ids allow only [a-zA-Z0-9_-]; other providers' ids may not. */
export function sanitizeToolId(id: string, cache: Map<string, string>): string {
	const existing = cache.get(id);
	if (existing) return existing;
	const clean = id.replace(/[^a-zA-Z0-9_-]/g, "_") || "tool";
	cache.set(id, clean);
	return clean;
}

/** System messages inside the conversation, as user text. */
export function systemAsUserText(content: string): string {
	return `<system-note>\n${content}\n</system-note>`;
}

/** Index of the first message of the trailing prompt: the run of user (and
 *  system) messages after the last assistant/tool message. Everything before it
 *  is session history; everything from it on is the prompt. One index for both
 *  halves so a message never lands in both (pcb's turnStart). */
export function turnStart(messages: readonly Message[]): number {
	let i = messages.length;
	while (i > 0 && (messages[i - 1].role === "user" || messages[i - 1].role === "system")) i--;
	return i;
}

/** The trailing prompt as text, or null when the history does not end in one. */
export function promptText(messages: readonly Message[]): string | null {
	const turn = messages.slice(turnStart(messages));
	if (turn.length === 0) return null;
	const text = turn
		.map((m) => (m.role === "system" ? systemAsUserText(m.content) : m.content))
		.filter((t) => t)
		.join("\n\n");
	return text || null;
}

export type Dropped = { abortedTurns: number; orphanResults: number };

/** Convert BubblePaw history to Anthropic messages (pcb's convertPiMessages). */
export function convertMessages(messages: readonly Message[], model?: string): {
	messages: SessionMessage[];
	sanitizedIds: Map<string, string>;
	dropped: Dropped;
} {
	const out: SessionMessage[] = [];
	const sanitizedIds = new Map<string, string>();
	const dropped: Dropped = { abortedTurns: 0, orphanResults: 0 };
	// The user message collecting this assistant turn's tool results and the index
	// of the assistant message it belongs to. All results of one turn must share the
	// user message directly after the assistant message, or repairToolPairing stubs
	// every result after the first (pcb convert.ts, the toolResult branch).
	let turnResults: { role: "user"; content: Array<any> } | null = null;
	let turnAssistantIdx: number | null = null;

	for (const msg of messages) {
		if (msg.role === "user") {
			out.push({ role: "user", content: msg.content || "[empty]" });
		} else if (msg.role === "system") {
			out.push({ role: "user", content: systemAsUserText(msg.content) });
		} else if (msg.role === "assistant") {
			const native = replayContent(msg, model);
			const blocks: Array<any> = native ?? [];
			if (native) {
				for (const block of blocks) if (block.type === "tool_use") {
					block.id = sanitizeToolId(block.id, sanitizedIds);
					if (!block.name.startsWith(MCP_TOOL_PREFIX)) block.name = sdkToolName(block.name);
				}
			} else {
				if (msg.content) blocks.push({ type: "text", text: msg.content });
				for (const call of msg.tool_calls ?? []) {
					blocks.push({
						type: "tool_use",
						id: sanitizeToolId(call.id, sanitizedIds),
						name: sdkToolName(call.name),
						input: call.arguments === undefined ? {} : call.arguments,
					});
				}
			}
			// A turn aborted before anything streamed has no content: inventing a
			// placeholder would land early in the prefix and cost the cache on every
			// rebuild. Drop it without clearing the turn bookkeeping (pcb).
			if (blocks.length === 0) {
				dropped.abortedTurns++;
				continue;
			}
			turnResults = null;
			turnAssistantIdx = out.length;
			out.push({ role: "assistant", content: blocks });
		} else if (msg.role === "tool") {
			const block = {
				type: "tool_result",
				tool_use_id: sanitizeToolId(msg.tool_call_id ?? "", sanitizedIds),
				content: msg.content ?? "",
				is_error: Boolean(msg.is_error),
			};
			if (turnAssistantIdx === null) dropped.orphanResults++;
			if (turnResults) {
				turnResults.content.push(block);
			} else {
				turnResults = { role: "user", content: [block] };
				// A result with no assistant before it is malformed history; appending
				// keeps it in order for repairToolPairing to discard.
				out.splice(turnAssistantIdx === null ? out.length : turnAssistantIdx + 1, 0, turnResults);
			}
		}
	}
	return { messages: out, sanitizedIds, dropped };
}

/** One-off (session-less) steps, e.g. summaries: pcb's isolated summary sends a
 *  single prompt. A lone user message is sent as is; anything longer is
 *  flattened to a labelled transcript. */
export function oneOffPrompt(messages: readonly Message[]): string {
	if (messages.length === 1 && messages[0].role === "user") return messages[0].content;
	return messages
		.map((m) => {
			if (m.role === "assistant") {
				const calls = (m.tool_calls ?? []).map((c) => `\n[tool call ${c.name} ${JSON.stringify(c.arguments)}]`).join("");
				return `assistant: ${m.content}${calls}`;
			}
			if (m.role === "tool") return `tool result${m.is_error ? " (error)" : ""}: ${m.content}`;
			return `${m.role}: ${m.content}`;
		})
		.join("\n\n");
}
