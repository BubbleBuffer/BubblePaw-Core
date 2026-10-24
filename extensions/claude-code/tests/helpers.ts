import type { Message, ModelRequest, ToolCall } from "../src/types.js";

let n = 0;
export function msg(role: Message["role"], content: string, extra: Partial<Message> = {}): Message {
	return { id: `m${++n}`, session_id: "s", role, content, tool_calls: [], tool_call_id: null, is_error: false, created_at: "t", ...extra };
}

export const user = (content: string) => msg("user", content);
export const system = (content: string) => msg("system", content);
export const assistant = (content: string, toolCalls: ToolCall[] = [], providerState?: unknown) =>
	msg("assistant", content, { tool_calls: toolCalls, provider_state: providerState });
/** The summarizer's per-step runtime note: an ephemeral message. */
export const note = (content: string) => msg("system", content, { id: "ephemeral:runtime-note" });
export const tool = (callId: string, content: string, isError = false) => msg("tool", content, { tool_call_id: callId, is_error: isError });

export const JS_TOOL = {
	name: "js",
	description: "Run JavaScript",
	input_schema: { type: "object", properties: { code: { type: "string" } }, required: ["code"] },
};

export function request(messages: Message[], epoch = "e1", extra: Partial<ModelRequest> = {}): ModelRequest {
	return {
		model: "haiku",
		system_prompt: "You are BubblePaw.",
		messages,
		tools: [JS_TOOL],
		max_output_tokens: null,
		session_id: "s",
		history: { epoch, rewritten_from: null },
		...extra,
	};
}

/** The assistant message the harness stores from a step response. */
export function stored(response: { text: string; tool_calls: ToolCall[]; provider_state?: unknown }): Message {
	return assistant(response.text, response.tool_calls, response.provider_state);
}
