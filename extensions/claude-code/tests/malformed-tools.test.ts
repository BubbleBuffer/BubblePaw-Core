import { expect, test } from "bun:test";
import { convertMessages } from "../src/convert.js";
import { ClaudeCodeProvider } from "../src/provider.js";
import { replayContent } from "../src/replay-state.js";
import { fakeQuery } from "./fake-sdk.js";
import { request, stored, tool, user } from "./helpers.js";

const usage = { input_tokens: 2, output_tokens: 7, cache_read_input_tokens: 3, cache_creation_input_tokens: 5 };

test("malformed streamed JSON keeps call id and usage with guaranteed-invalid null arguments, then releases only the host error", async () => {
	const fake = fakeQuery([{ sessionId: "cc", turns: [
		{ tools: [{ id: "malformed", name: "js", input: {} }], partialJson: { malformed: '{"code":' }, usage },
		{ text: "corrected" },
	] }]);
	const logs: string[] = [];
	const provider = new ClaudeCodeProvider({ query: fake.fn, log: (line) => logs.push(line), writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const firstUser = user("go");
	// An empty object is valid for this tool; the old parse-failure fallback
	// could therefore pass host validation and execute it.
	const objectTool = { name: "js", description: "No required arguments", input_schema: { type: "object" } };
	const first = await provider.step(request([firstUser], "e1", { tools: [objectTool] }), 1, () => {});
	expect(first.tool_calls).toEqual([{ id: "malformed", name: "js", arguments: null }]);
	expect(first.usage).toEqual({ input_tokens: 10, output_tokens: 7, cache_read_tokens: 3, cache_creation_tokens: 5 });
	expect(logs).toContain("unparseable tool input for malformed");
	const saved = JSON.parse(JSON.stringify(stored(first)));
	expect(replayContent(saved, "haiku")?.[0].input).toBeNull();
	expect((convertMessages([saved], "haiku").messages[0].content as any[])[0].input).toBeNull();
	const second = await provider.step(request([firstUser, saved, tool("malformed", "Invalid arguments", true)], "e1", { tools: [objectTool] }), 2, () => {});
	expect(second.text).toBe("corrected");
	expect(fake.runs[0].toolResults).toEqual([{ id: "malformed", result: { content: [{ type: "text", text: "Invalid arguments" }], isError: true } }]);
	expect(fake.runs.length).toBe(1);
});

for (const nonStreaming of [false, true]) test(`${nonStreaming ? "fallback" : "stream"} schema-invalid object reaches host validation unchanged; MCP parks until host rejects`, async () => {
	const fake = fakeQuery([{ sessionId: "cc", turns: [{ nonStreaming, tools: [{ id: "invalid", name: "js", input: { code: 7 } }], usage }, { text: "retry safely" }] }]);
	const provider = new ClaudeCodeProvider({ query: fake.fn, writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const firstUser = user("go");
	const first = await provider.step(request([firstUser]), 1, () => {});
	expect(first.tool_calls).toEqual([{ id: "invalid", name: "js", arguments: { code: 7 } }]);
	expect(first.usage?.output_tokens).toBe(7);
	expect(fake.runs[0].toolResults).toEqual([]);
	const second = await provider.step(request([firstUser, stored(first), tool("invalid", "code must be a string", true)]), 2, () => {});
	expect(second.text).toBe("retry safely");
	expect(fake.runs[0].toolResults[0].result.isError).toBe(true);
	expect(fake.runs[0].toolResults[0].result.content[0].text).toBe("code must be a string");
});

test("explicit native null arguments remain null in fallback and ordinary rebuild", async () => {
	const fake = fakeQuery([{ sessionId: "cc", turns: [{ nonStreaming: true, blocks: [{ type: "tool_use", id: "null", name: "mcp__bubblepaw__js", input: null }], tools: [{ id: "null", name: "js", input: {} }] }, { text: "done" }] }]);
	const provider = new ClaudeCodeProvider({ query: fake.fn, writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const firstUser = user("go");
	const first = await provider.step(request([firstUser]), 1, () => {});
	expect(first.tool_calls[0].arguments).toBeNull();
	const saved = stored(first);
	const ordinary = { ...saved, provider_state: undefined };
	expect((convertMessages([ordinary]).messages[0].content as any[])[0].input).toBeNull();
	await provider.step(request([firstUser, saved, tool("null", "Invalid arguments", true)]), 2, () => {});
});

test("nullable root tool schemas fail before query, preserving the malformed null sentinel guarantee", async () => {
	const fake = fakeQuery([]);
	const provider = new ClaudeCodeProvider({ query: fake.fn, writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const error = await provider.step(request([user("go")], "e1", { tools: [{ name: "nullable", description: "invalid MCP schema", input_schema: { type: ["object", "null"] } }] }), 1, () => {}).catch((failure) => failure);
	expect(error.kind).toBe("fatal");
	expect(fake.runs).toEqual([]);
});

test("streamed tool block cut before content_block_stop is metered with invalid null, even when its partial input parses", async () => {
	const fake = fakeQuery([{ sessionId: "cc", turns: [{ tools: [{ id: "cut", name: "js", input: {} }], openToolBlocks: ["cut"], usage }, { text: "done" }] }]);
	const provider = new ClaudeCodeProvider({ query: fake.fn, writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const objectTool = { name: "js", description: "No required arguments", input_schema: { type: "object" } };
	const firstUser = user("go");
	const first = await provider.step(request([firstUser], "e1", { tools: [objectTool] }), 1, () => {});
	expect(first.tool_calls).toEqual([{ id: "cut", name: "js", arguments: null }]);
	expect(first.usage?.output_tokens).toBe(7);
	await provider.step(request([firstUser, stored(first), tool("cut", "Invalid arguments", true)], "e1", { tools: [objectTool] }), 2, () => {});
	expect(fake.runs[0].toolResults[0].result.isError).toBe(true);
});

test("completed fallback empty-object call stays valid for an object tool with no required arguments", async () => {
	const fake = fakeQuery([{ sessionId: "cc", turns: [{ nonStreaming: true, tools: [{ id: "valid", name: "js", input: {} }] }, { text: "done" }] }]);
	const provider = new ClaudeCodeProvider({ query: fake.fn, writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const objectTool = { name: "js", description: "No required arguments", input_schema: { type: "object" } };
	const firstUser = user("go");
	const first = await provider.step(request([firstUser], "e1", { tools: [objectTool] }), 1, () => {});
	expect(first.tool_calls).toEqual([{ id: "valid", name: "js", arguments: {} }]);
	await provider.step(request([firstUser, stored(first), tool("valid", "ok")], "e1", { tools: [objectTool] }), 2, () => {});
});

test("completed native fallback without input is retained as invalid null instead of executable empty object", async () => {
	const fake = fakeQuery([{ sessionId: "cc", turns: [{ nonStreaming: true, blocks: [{ type: "tool_use", id: "missing", name: "mcp__bubblepaw__js" }], tools: [{ id: "missing", name: "js", input: {} }] }, { text: "done" }] }]);
	const provider = new ClaudeCodeProvider({ query: fake.fn, writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const firstUser = user("go");
	const first = await provider.step(request([firstUser]), 1, () => {});
	expect(first.tool_calls).toEqual([{ id: "missing", name: "js", arguments: null }]);
	await provider.step(request([firstUser, stored(first), tool("missing", "Invalid arguments", true)]), 2, () => {});
});

for (const fixture of [
	{ name: "missing native input and no deltas", native: {}, expected: null },
	{ name: "explicit empty native object and no deltas", native: { input: {} }, expected: {} },
	{ name: "missing provisional input completed by valid JSON delta", native: {}, delta: "{}", expected: {} },
] as const) test(`completed streamed tool ${fixture.name}`, async () => {
	async function* events() {
		yield { type: "stream_event", event: { type: "message_start", message: { id: "native-missing", usage: { input_tokens: 2 } } } };
		yield { type: "stream_event", event: { type: "content_block_start", index: 0, content_block: { type: "tool_use", id: "native", name: "mcp__bubblepaw__js", ...fixture.native } } };
		if ("delta" in fixture) yield { type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: fixture.delta } } };
		yield { type: "stream_event", event: { type: "content_block_stop", index: 0 } };
		yield { type: "stream_event", event: { type: "message_delta", usage: { output_tokens: 3 } } };
		yield { type: "stream_event", event: { type: "message_stop" } };
		yield { type: "result", subtype: "success", result: "", is_error: false };
	}
	const provider = new ClaudeCodeProvider({ query: () => events(), writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const first = await provider.step(request([user("go")]), 1, () => {});
	expect(first.tool_calls).toEqual([{ id: "native", name: "js", arguments: fixture.expected }]);
	expect(first.usage?.output_tokens).toBe(3);
});
