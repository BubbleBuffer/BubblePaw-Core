import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { convertMessages } from "../src/convert.js";
import { writeSession } from "../src/mirror.js";
import { ClaudeCodeProvider } from "../src/provider.js";
import { replayContent } from "../src/replay-state.js";
import { fakeQuery } from "./fake-sdk.js";
import { request, stored, tool, user } from "./helpers.js";

function canonical(value: any): any {
	if (Array.isArray(value)) return value.map(canonical);
	if (value && typeof value === "object") return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonical(value[key])]));
	return value;
}
const hash = (value: unknown) => createHash("sha256").update(JSON.stringify(canonical(value))).digest("hex");
const blocks: any[] = [
	{ type: "thinking", thinking: "Plan 猫\nπ 🐾", signature: "sig+/==猫".repeat(800), extra: { signedZero: -0, version: 7 } },
	{ type: "text", text: "α🐾", citations: [{ unknown: "retained" }] },
	{ type: "redacted_thinking", data: "cipher+/==\nπ".repeat(900), opaque: [true, { untouched: "猫" }] },
	{ type: "tool_use", id: "toolu_signed", name: "mcp__bubblepaw__js", input: { code: "π", data: "once" }, opaque: { keep: 1 } },
	{ type: "thinking", thinking: "After call", signature: "tail-signature+/=" },
	{ type: "text", text: "β\n", other: ["field"] },
];

for (const nonStreaming of [false, true]) test(`${nonStreaming ? "fallback" : "stream"} signed opaque blocks survive canonical JSON and ordered rebuild`, async () => {
	const fake = fakeQuery([{ sessionId: "signed-session", turns: [{ blocks, nonStreaming, tools: [{ id: "toolu_signed", name: "js", input: blocks[3].input }] }] }]);
	const provider = new ClaudeCodeProvider({ query: fake.fn, writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const text: string[] = [];
	const response = await provider.step(request([user("go")]), 1, (delta) => text.push(delta));
	expect(response.text).toBe("α🐾β\n");
	expect(text.join("")).toBe(response.text);
	const message = JSON.parse(JSON.stringify(stored(response)));
	const state = message.provider_state;
	expect(state._bubblepaw_origin).toEqual({ provider_kind: "anthropic-messages", model: "haiku", source: "native" });
	// The signed payload has one owner and text/calls have references, not mirrors.
	expect(JSON.stringify(state).split(blocks[0].signature).length - 1).toBe(1);
	expect(state.content[1].text).toBeUndefined();
	expect(state.content[3].input).toBeUndefined();
	expect(state.content[3].id).toBeUndefined();
	const expected = structuredClone(blocks);
	expected[3].name = "js";
	expect(hash(replayContent(message, "haiku"))).toBe(hash(expected));
	const converted = convertMessages([message, tool("toolu_signed", "result")], "haiku").messages;
	expect(hash(converted[0].content)).toBe(hash(blocks));
	const dir = mkdtempSync(join(tmpdir(), "bubblepaw-opaque-"));
	try {
		const written = writeSession({ priors: [user("go"), message, tool("toolu_signed", "result")], cwd: dir, claudeDir: dir, previousId: "00000000-0000-4000-8000-000000000001", model: "haiku" });
		expect(written.warnings).toEqual([]);
		// Find only our synthetic session file; never inspect real SDK transcripts.
		function records(path: string): any[] {
			return readdirSync(path, { withFileTypes: true }).flatMap((entry) => entry.isDirectory()
				? records(join(path, entry.name))
				: entry.name.endsWith(".jsonl") ? readFileSync(join(path, entry.name), "utf8").trim().split("\n").map((line) => JSON.parse(line)) : []);
		}
		const saved = records(dir).find((record) => record.type === "assistant");
		expect(saved).toBeDefined();
		expect(hash(saved.message.content)).toBe(hash(blocks));
	} finally { rmSync(dir, { recursive: true, force: true }); provider.closeSession("s"); }
});

for (const nonStreaming of [false, true]) for (const withThinking of [false, true]) {
	test(`${nonStreaming ? "fallback" : "stream"} tool result delivery ${withThinking ? "retains thinking" : "works without thinking"}`, async () => {
		const firstBlocks = [
			...(withThinking ? [blocks[0], blocks[2]] : []),
			{ type: "text", text: "Calling" },
			blocks[3],
		];
		const fake = fakeQuery([{ sessionId: "deliver-session", turns: [
			{ blocks: firstBlocks, nonStreaming, tools: [{ id: "toolu_signed", name: "js", input: blocks[3].input }] },
			{ text: "42", nonStreaming },
		] }]);
		const writes: any[] = [];
		const paths: string[] = [];
		const provider = new ClaudeCodeProvider({
			query: fake.fn,
			writeSession: (opts) => { writes.push(opts); return { sessionId: null, records: 0, warnings: [] }; },
			onStep: (step) => paths.push(step.path),
		});
		try {
			const start = user("go");
			const first = stored(await provider.step(request([start]), 1, () => {}));
			const stateBefore = hash(first.provider_state);
			const expected = structuredClone(firstBlocks);
			expected[expected.length - 1].name = "js";
			expect(hash(replayContent(first, "haiku"))).toBe(hash(expected));
			const opaque = (first.provider_state as any).content.filter((block: any) =>
				block.type === "thinking" || block.type === "redacted_thinking");
			expect(hash(opaque)).toBe(hash(withThinking ? [blocks[0], blocks[2]] : []));
			const response = await provider.step(request([start, first, tool("toolu_signed", "42")]), 2, () => {});
			expect(response.text).toBe("42");
			expect(response.tool_calls).toEqual([]);
			expect(paths).toEqual(["fresh", "deliver"]);
			expect(writes.length).toBe(1);
			expect(fake.runs.length).toBe(1);
			expect(fake.runs[0].toolResults.length).toBe(1);
			expect(fake.runs[0].toolResults[0].id).toBe("toolu_signed");
			expect(hash(first.provider_state)).toBe(stateBefore);
		} finally { provider.closeSession("s"); }
	});
}

test("foreign kind and known model omit opaque state without modifying canonical data", async () => {
	const fake = fakeQuery([{ sessionId: "s", turns: [{ blocks: blocks.filter((b) => b.type !== "tool_use"), text: "α🐾β\n" }] }]);
	const provider = new ClaudeCodeProvider({ query: fake.fn, writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
	const message = stored(await provider.step(request([user("go")]), 1, () => {}));
	const before = hash(message);
	expect(convertMessages([message], "other-model").messages[0].content).toEqual([{ type: "text", text: message.content }]);
	expect(hash(message)).toBe(before);
	const foreign = structuredClone(message);
	(foreign.provider_state as any)._bubblepaw_origin.provider_kind = "responses";
	expect(convertMessages([foreign], "haiku").messages[0].content).toEqual([{ type: "text", text: foreign.content }]);
	provider.closeSession("s");
});

test("reuse keeps signed SDK session, model change rebuilds without foreign thinking", async () => {
	const fake = fakeQuery([
		{ sessionId: "signed", turns: [{ blocks: blocks.filter((b) => b.type !== "tool_use"), text: "α🐾β\n" }] },
		{ sessionId: "signed", turns: [{ text: "next" }] },
		{ sessionId: "rebuilt", turns: [{ text: "different model" }] },
	]);
	const writes: any[] = [];
	const provider = new ClaudeCodeProvider({ query: fake.fn, writeSession: (opts) => { writes.push(opts); return { sessionId: opts.priors.length ? "rebuilt" : null, records: 0, warnings: [] }; } });
	const start = user("go");
	const first = stored(await provider.step(request([start]), 1, () => {}));
	const history = [start, first, user("more")];
	const second = stored(await provider.step(request(history), 2, () => {}));
	expect(fake.runs[1].options.resume).toBe("signed");
	expect(writes.length).toBe(1);
	await provider.step(request([...history, second, user("new model")], "e1", { model: "other" }), 3, () => {});
	expect(writes.length).toBe(2);
	expect(writes[1].model).toBe("other");
	const converted = convertMessages(writes[1].priors, "other");
	expect(JSON.stringify(converted.messages)).not.toContain("signature");
	provider.closeSession("s");
});

test("sessionless stream retains signed thinking too", async () => {
	async function* events() {
		yield { type: "stream_event", event: { type: "message_start", message: { id: "summary" } } };
		yield { type: "stream_event", event: { type: "content_block_start", index: 0, content_block: blocks[0] } };
		yield { type: "stream_event", event: { type: "content_block_stop", index: 0 } };
		yield { type: "stream_event", event: { type: "content_block_start", index: 1, content_block: { type: "text", text: "" } } };
		yield { type: "stream_event", event: { type: "content_block_delta", index: 1, delta: { type: "text_delta", text: "summary 猫" } } };
		yield { type: "stream_event", event: { type: "content_block_stop", index: 1 } };
		yield { type: "stream_event", event: { type: "message_stop" } };
		yield { type: "result", subtype: "success", result: "summary 猫", is_error: false };
	}
	const provider = new ClaudeCodeProvider({ query: () => events() });
	const response = await provider.step(request([user("summarize")], "e1", { session_id: undefined }), 1, () => {});
	expect(hash(replayContent(stored(response), "haiku")?.[0])).toBe(hash(blocks[0]));
});
