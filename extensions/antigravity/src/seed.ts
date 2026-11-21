// BubblePaw messages -> text for agy. agy sessions take plain user text, so a new
// session is seeded with the request's history rendered as a role-labelled
// transcript in its first prompt, the current user input last. When the transcript
// is large the input is repeated at the top: a long seed with the question only at
// the end was read unreliably in the 2026-11-21 probe (seeds of 75k+ tokens; the
// question at both ends worked).

import type { Message } from "./types.js";

/** Characters of transcript above which the current input is repeated at the top. */
export const REPEAT_ABOVE_CHARS = 20_000;

export const isEphemeral = (m: Message) => m.id.startsWith("ephemeral:");

export function systemAsText(content: string): string {
	return `<system-note>\n${content}\n</system-note>`;
}

/** Index of the first message of the trailing prompt: the run of user (and system)
 *  messages after the last assistant/tool message. */
export function turnStart(messages: readonly Message[]): number {
	let i = messages.length;
	while (i > 0 && (messages[i - 1].role === "user" || messages[i - 1].role === "system")) i--;
	return i;
}

/** User and system messages as one prompt text, or "" when there are none. */
export function promptText(messages: readonly Message[]): string {
	return messages
		.map((m) => (m.role === "system" ? systemAsText(m.content) : m.content))
		.filter((t) => t)
		.join("\n\n");
}

function renderOne(m: Message): string {
	if (m.role === "assistant") {
		const calls = (m.tool_calls ?? []).map((c) => `\n[tool call ${c.id}: ${c.name} ${JSON.stringify(c.arguments ?? {})}]`).join("");
		return `assistant: ${m.content}${calls}`;
	}
	if (m.role === "tool") return `tool result ${m.tool_call_id ?? ""}${m.is_error ? " (error)" : ""}: ${m.content}`;
	if (m.role === "system") return systemAsText(m.content);
	return `user: ${m.content}`;
}

/** The first prompt of a new session: the earlier conversation as a transcript,
 *  then the current user input. A request that ends in a tool result (no user
 *  input after it) ends with a continue instruction instead. */
export function seedPrompt(messages: readonly Message[]): string {
	const cut = turnStart(messages);
	const history = messages.slice(0, cut);
	const current = promptText(messages.slice(cut));
	if (history.length === 0) return current || "[continue]";
	const transcript = history.map(renderOne).join("\n\n");
	const closing = current ? `Current user input:\n${current}` : "Continue from the last tool result above.";
	const head = current && transcript.length > REPEAT_ABOVE_CHARS ? `The current user input is repeated at the end of this message: ${current}\n\n` : "";
	return `${head}This conversation is already in progress. Earlier turns:\n\n<conversation_so_far>\n${transcript}\n</conversation_so_far>\n\n${closing}`;
}

/** One-off prompt for a summary fork: a lone user message as it is, anything
 *  longer as a labelled transcript. */
export function forkPrompt(messages: readonly Message[]): string {
	if (messages.length === 1 && messages[0].role === "user") return messages[0].content;
	return seedPrompt(messages);
}
