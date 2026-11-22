import { mkdtempSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { launchers } from "../src/main.js";
import { AntigravityProvider } from "../src/provider.js";
import type { Message, ModelRequest, ToolCall } from "../src/types.js";

let n = 0;
export function msg(role: Message["role"], content: string, extra: Partial<Message> = {}): Message {
	return { id: `m${++n}`, session_id: "s", role, content, tool_calls: [], tool_call_id: null, is_error: false, created_at: "t", ...extra };
}
export const user = (content: string) => msg("user", content);
export const assistant = (content: string, toolCalls: ToolCall[] = [], providerState?: unknown) => msg("assistant", content, { tool_calls: toolCalls, provider_state: providerState });
export const tool = (callId: string, content: string, isError = false) => msg("tool", content, { tool_call_id: callId, is_error: isError });

export const LOOKUP = { name: "lookup", description: "Look something up", input_schema: { type: "object", properties: { name: { type: "string" } } } };

export function request(messages: Message[], epoch = "e1", extra: Partial<ModelRequest> = {}): ModelRequest {
	return { model: "gemini-3.8-flash", system_prompt: "You are BubblePaw.", messages, tools: [LOOKUP], max_output_tokens: null, session_id: "s", history: { epoch, rewritten_from: null }, ...extra };
}

/** The assistant message the host stores from a step response. */
export function stored(response: { text: string; tool_calls: ToolCall[]; provider_state?: unknown }): Message {
	return assistant(response.text, response.tool_calls, response.provider_state);
}

/** A provider whose `agy` is the scripted fake; `received()` lists what the fake saw. */
export function fakeProvider(settleMs = 50) {
	const dir = mkdtempSync(join(tmpdir(), "bp-agy-test-"));
	const logFile = join(dir, "fake.log");
	const provider = new AntigravityProvider({
		command: process.execPath,
		args: [join(import.meta.dir, "fake-agy.ts")],
		cwd: join(dir, "workspace"),
		env: { ...(process.env as Record<string, string>), FAKE_AGY_LOG: logFile },
		shim: launchers().shim,
		settleMs,
		log: () => {},
	});
	const received = (): any[] => (existsSync(logFile) ? readFileSync(logFile, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l)) : []);
	const calls = (method: string) => received().filter((r) => r.method === method);
	return { provider, received, calls, dir };
}
