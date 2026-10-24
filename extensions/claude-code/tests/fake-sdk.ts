// A fake Agent SDK `query()` that behaves like Claude Code for the step engine:
// it reads the prompt, streams scripted assistant messages, calls tools through
// the in-process MCP server (with the claudecode/toolUseId _meta) and waits on
// them, then ends with a `result`.

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import type { SDKResultSuccess } from "@anthropic-ai/claude-agent-sdk";
import { TOOL_USE_ID_META } from "../src/mcp-server.js";

export interface FakeUsage {
	input_tokens: number;
	cache_read_input_tokens: number;
	cache_creation_input_tokens?: number;
	output_tokens: number;
	output_tokens_details?: { thinking_tokens: number } | null;
}

export type FakeModelUsage = Record<string, { inputTokens: number; outputTokens: number }>;

export interface ScriptTurn {
	/** The full model ID the SDK reports on this message (default `claude-haiku-5-5`);
	 *  an explicit `undefined` omits the field, as an unnamed message would. */
	model?: string;
	/** Omit usage from every frame of this message; the model is still named. */
	noUsage?: boolean;
	/** Exact native order, including signed thinking and unknown opaque fields. */
	blocks?: any[];
	text?: string;
	tools?: Array<{ id: string; name: string; input: unknown }>;
	usage?: FakeUsage;
	/** Deliver as a single non-streamed `assistant` message (Claude Code's fallback). */
	nonStreaming?: boolean;
	/** Raw input_json_delta payload keyed by tool_use id (malformed wire tests). */
	partialJson?: Record<string, string>;
	/** Omit content_block_stop for these calls to simulate a truncated block. */
	openToolBlocks?: string[];
}

export interface FakeRun {
	options: any;
	prompt: string;
	toolResults: Array<{ id: string; result: any }>;
	/** Prompt-stream messages after the first (steering), with their priority. */
	steers: Array<{ text: string; priority?: string }>;
	/** Order of arrivals: `steer:<text>` and `result:<tool_use id>`. */
	events: string[];
	interrupted: boolean;
	closed: boolean;
}

export interface FakeScript {
	sessionId: string;
	turns: ScriptTurn[];
	rateLimits?: any[];
	/** Instead of a success result: */
	error?: { result: string; rateLimit?: { resetsAt: number; rateLimitType?: string }; assistantError?: string; errors?: string[]; subtype?: string;
		/** Usage and per-model totals an error result still carries. */
		usage?: FakeUsage; modelUsage?: FakeModelUsage };
	/** Never finish the first turn (for cancel tests). */
	hang?: boolean;
	/** SDK success-result fields, including native structured output and aggregate usage. */
	result?: Pick<SDKResultSuccess, "structured_output"> & { result?: string; usage?: FakeUsage; modelUsage?: FakeModelUsage };
	/** Throw after publishing scripted messages, rather than yielding a result. */
	throwAfterTurns?: string;
}

export function fakeQuery(scripts: FakeScript[]) {
	const runs: FakeRun[] = [];
	let next = 0;
	const fn = ({ prompt, options }: { prompt: any; options: any }) => {
		const script = scripts[next++];
		if (!script) throw new Error("fake query: no script left");
		const run: FakeRun = { options, prompt: "", toolResults: [], steers: [], events: [], interrupted: false, closed: false };
		runs.push(run);
		let wake: (() => void) | null = null;
		const stop = () => {
			wake?.();
		};
		async function* gen() {
			if (typeof prompt === "string") run.prompt = prompt;
			else {
				const first = await prompt.next();
				const content = first.value?.message?.content;
				run.prompt = Array.isArray(content) ? content.map((b: any) => b.text ?? "").join("") : String(content ?? "");
			}
			// Keep reading the prompt stream, as the SDK pumps it to stdin: every
			// later message is a steer, acknowledged by asking for the next one.
			void (async () => {
				if (typeof prompt === "string") return;
				try {
					while (true) {
						const next = await prompt.next();
						if (next.done) return;
						const c = next.value?.message?.content;
						const text = Array.isArray(c) ? c.map((b: any) => b.text ?? "").join("") : String(c ?? "");
						run.steers.push({ text, priority: next.value?.priority });
						run.events.push(`steer:${text}`);
					}
				} catch {}
			})();
			yield { type: "system", subtype: "init", session_id: script.sessionId };
			for (const info of script.rateLimits ?? []) yield { type: "rate_limit_event", rate_limit_info: info };
			let client: Client | null = null;
			const server = options.mcpServers?.bubblepaw?.instance;
			if (server) {
				const [a, b] = InMemoryTransport.createLinkedPair();
				await server.connect(a);
				client = new Client({ name: "fake-cc", version: "1" });
				await client.connect(b);
			}
			if (script.hang) {
				yield { type: "stream_event", session_id: script.sessionId, event: { type: "message_start", message: { id: "msg_hang", usage: { input_tokens: 1 } } } };
				await new Promise<void>((resolve) => {
					wake = resolve;
				});
				return;
			}
			let n = 0;
			for (const turn of script.turns) {
				if (run.interrupted) return;
				const id = `msg_${script.sessionId}_${n++}`;
				const model = "model" in turn ? turn.model : "claude-haiku-5-5";
				const usage = turn.noUsage ? undefined : (turn.usage ?? { input_tokens: 10, cache_read_input_tokens: 0, output_tokens: 5 });
				const blocks: any[] = structuredClone(turn.blocks ?? []);
				if (!turn.blocks) {
					if (turn.text) blocks.push({ type: "text", text: turn.text });
					for (const tool of turn.tools ?? []) blocks.push({ type: "tool_use", id: tool.id, name: `mcp__bubblepaw__${tool.name}`, input: tool.input });
				}
				if (turn.nonStreaming) {
					yield { type: "assistant", session_id: script.sessionId, message: { id, model, content: blocks, ...(usage ? { usage } : {}) } };
				} else {
					const { output_tokens, output_tokens_details, ...startUsage } = (usage ?? {}) as Partial<FakeUsage>;
					yield { type: "stream_event", session_id: script.sessionId, event: { type: "message_start", message: { id, model, ...(usage ? { usage: { ...startUsage, output_tokens: 1 } } : {}) } } };
					let index = 0;
					for (const block of blocks) {
						if (block.type === "text") {
							yield { type: "stream_event", event: { type: "content_block_start", index, content_block: { ...block, text: "" } } };
							for (const piece of block.text.match(/[\s\S]{1,4}/gu) ?? []) {
								yield { type: "stream_event", event: { type: "content_block_delta", index, delta: { type: "text_delta", text: piece } } };
							}
						} else if (block.type === "tool_use") {
							yield { type: "stream_event", event: { type: "content_block_start", index, content_block: { ...block, input: {} } } };
							yield { type: "stream_event", event: { type: "content_block_delta", index, delta: { type: "input_json_delta", partial_json: turn.partialJson?.[block.id] ?? JSON.stringify(block.input) } } };
						} else if (block.type === "thinking") {
							yield { type: "stream_event", event: { type: "content_block_start", index, content_block: { ...block, thinking: "", signature: "" } } };
							for (const piece of block.thinking.match(/[\s\S]{1,4}/gu) ?? []) yield { type: "stream_event", event: { type: "content_block_delta", index, delta: { type: "thinking_delta", thinking: piece } } };
							for (const piece of block.signature.match(/[\s\S]{1,3}/gu) ?? []) yield { type: "stream_event", event: { type: "content_block_delta", index, delta: { type: "signature_delta", signature: piece } } };
						} else {
							yield { type: "stream_event", event: { type: "content_block_start", index, content_block: block } };
						}
						if (block.type !== "tool_use" || !turn.openToolBlocks?.includes(block.id)) yield { type: "stream_event", event: { type: "content_block_stop", index } };
						index++;
					}
					yield { type: "stream_event", event: { type: "message_delta", delta: { stop_reason: turn.tools?.length ? "tool_use" : "end_turn" }, ...(usage ? { usage: { output_tokens, ...(output_tokens_details !== undefined ? { output_tokens_details } : {}) } } : {}) } };
					yield { type: "stream_event", event: { type: "message_stop" } };
					yield { type: "assistant", session_id: script.sessionId, message: { id, model, content: blocks, ...(usage ? { usage } : {}) } };
				}
				if (turn.tools?.length && client) {
					// Claude Code may run MCP calls one after another; do that, so a
					// result delivered before its call is parked exercises the queue.
					for (const tool of turn.tools) {
						const result = await client.callTool({ name: tool.name, arguments: tool.input as any, _meta: { [TOOL_USE_ID_META]: tool.id } });
						run.toolResults.push({ id: tool.id, result });
						run.events.push(`result:${tool.id}`);
					}
					yield { type: "user", session_id: script.sessionId, message: { role: "user", content: [] } };
				}
			}
			if (script.throwAfterTurns !== undefined) throw new Error(script.throwAfterTurns);
			if (script.error) {
				if (script.error.rateLimit) yield { type: "rate_limit_event", rate_limit_info: { status: "rejected", ...script.error.rateLimit } };
				if (script.error.assistantError) yield { type: "assistant", error: script.error.assistantError, session_id: script.sessionId, message: { id: "syn", model: "<synthetic>", content: [{ type: "text", text: script.error.result }] } };
				yield { type: "result", subtype: script.error.subtype ?? "success", is_error: true, result: script.error.result, session_id: script.sessionId, ...(script.error.errors ? { errors: script.error.errors } : {}),
					...(script.error.usage ? { usage: script.error.usage } : {}), ...(script.error.modelUsage ? { modelUsage: script.error.modelUsage } : {}) };
				return;
			}
			yield { type: "result", subtype: "success", is_error: false, result: script.turns.at(-1)?.text ?? "", session_id: script.sessionId, ...script.result };
		}
		const iterator = gen();
		return Object.assign(iterator, {
			interrupt: async () => {
				run.interrupted = true;
				stop();
			},
			close: () => {
				run.closed = true;
				stop();
			},
		});
	};
	return { fn, runs };
}
