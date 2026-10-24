import { describe, expect, test } from "bun:test";
import { ClaudeCodeProvider, type StepLog } from "../src/provider.js";
import { replayContent } from "../src/replay-state.js";
import type { ModelRequest } from "../src/types.js";
import { fakeQuery, type FakeUsage } from "./fake-sdk.js";
import { request, stored, tool, user } from "./helpers.js";

const schema = {
	name: "answer_v1",
	schema: { type: "object", properties: { answer: { type: "integer" } }, required: ["answer"], additionalProperties: false },
};

function setup(scripts: Parameters<typeof fakeQuery>[0]) {
	const fake = fakeQuery(scripts);
	const writes: unknown[] = [];
	const steps: StepLog[] = [];
	const provider = new ClaudeCodeProvider({ query: fake.fn, cwd: ".", onStep: (step) => steps.push(step),
		writeSession: (opts) => {
			writes.push(opts);
			return { sessionId: opts.priors.length ? "rebuilt" : null, records: 0, warnings: [] };
		} });
	return { provider, fake, writes, steps };
}

describe("native effort and structured output", () => {
	for (const sessionless of [false, true]) {
		for (const effort of [undefined, "low", "medium", "high", "xhigh", "max"] as const) {
			test(`${sessionless ? "sessionless" : "stateful"} maps ${effort ?? "default low"} effort and keeps reasoning on`, async () => {
				const { provider, fake } = setup([{ sessionId: "cc", turns: [{ text: "ok" }] }]);
				await provider.step(request([user("go")], "e1", { reasoning_effort: effort, ...(sessionless ? { session_id: undefined } : {}) }), 1, () => {});
				expect(fake.runs[0].options.effort).toBe(effort ?? "low");
				expect(fake.runs[0].options.thinking).toEqual({ type: "adaptive" });
				if (sessionless) {
					expect(fake.runs[0].prompt).toBe("go");
					expect(fake.runs[0].options.persistSession).toBe(false);
				}
			});
		}

		for (const nonStreaming of [false, true]) test(`${sessionless ? "sessionless" : "stateful"} ${nonStreaming ? "fallback" : "stream"} uses native schema and serialized typed result`, async () => {
			const opaque = { type: "thinking", thinking: "choose an answer", signature: "signed+/=", opaque: { keep: true } };
			const blocks = [opaque, { type: "text", text: "formatting now", metadata: { keep: true } },
				{ type: "tool_use", id: "format", name: "StructuredOutput", input: { answer: 42 } },
				{ type: "redacted_thinking", data: "cipher+/=" }, { type: "text", text: "done", extra: 7 }];
			const { provider, fake } = setup([{ sessionId: "cc", turns: [{ blocks, nonStreaming }], result: { structured_output: { answer: 42 } } }]);
			const deltas: string[] = [];
			const response = await provider.step(request([user("answer")], "e1", { output_schema: schema, ...(sessionless ? { session_id: undefined } : {}) }), 1, (text) => deltas.push(text));
			expect(fake.runs[0].options.outputFormat).toEqual({ type: "json_schema", schema: schema.schema });
			const reply: { answer: number } = JSON.parse(response.text);
			expect(reply).toEqual({ answer: 42 });
			expect(deltas.join("")).toBe(response.text);
			expect(response.tool_calls).toEqual([]);
			const native = replayContent(stored(response), "haiku")!;
			expect(native[0]).toEqual(opaque);
			expect(native[1]).toEqual({ type: "text", text: response.text, metadata: { keep: true } });
			expect(native[2]).toEqual({ type: "redacted_thinking", data: "cipher+/=" });
			expect(native[3]).toEqual({ type: "text", text: "", extra: 7 });
		});

		test(`${sessionless ? "sessionless" : "stateful"} missing native structured output fails explicitly`, async () => {
			const { provider } = setup([{ sessionId: "cc", turns: [{ text: "ordinary prose" }] }]);
			const deltas: string[] = [];
			const error = await provider.step(request([user("answer")], "e1", { output_schema: schema, ...(sessionless ? { session_id: undefined } : {}) }), 1, (text) => deltas.push(text)).catch((failure) => failure);
			expect(error.kind).toBe("fatal");
			expect(error.message).toContain("output_schema");
			expect(deltas).toEqual([]);
		});

		for (const [name, invalid] of [
			["effort", { reasoning_effort: "extreme" }],
			["schema name", { output_schema: { ...schema, name: "invalid name" } }],
			["nonobject schema", { output_schema: { name: "answer", schema: [] } }],
			["open root object", { output_schema: { name: "answer", schema: { type: "object" } } }],
			["open nested object", { output_schema: { name: "answer", schema: { type: "array", items: { type: "object" } } } }],
		] as const) test(`${sessionless ? "sessionless" : "stateful"} invalid ${name} fails before query or session write`, async () => {
			const { provider, fake, writes } = setup([]);
			const error = await provider.step(request([user("go")], "e1", { ...(invalid as unknown as Partial<ModelRequest>), ...(sessionless ? { session_id: undefined } : {}) }), 1, () => {}).catch((failure) => failure);
			expect(error.kind).toBe("fatal");
			expect(fake.runs).toEqual([]);
			expect(writes).toEqual([]);
		});
	}

	for (const option of ["reasoning_effort", "output_schema"] as const) test(`parked changed ${option} fails before releasing tools and unchanged delivery remains possible`, async () => {
		const { provider, fake, writes } = setup([{ sessionId: "cc", turns: [
			{ tools: [{ id: "call", name: "js", input: {} }] }, { text: "formatting" },
		], result: { structured_output: { answer: 42 } } }]);
		const firstUser = user("go");
		const options = { reasoning_effort: "low", output_schema: schema } as const;
		const first = await provider.step(request([firstUser], "e1", options), 1, () => {});
		const history = [firstUser, stored(first), tool("call", "ok"), user("steer me")];
		const changed = option === "reasoning_effort" ? { reasoning_effort: "high" as const } : { output_schema: { ...schema, name: "changed" } };
		const error = await provider.step(request(history, "e1", { ...options, ...changed }), 2, () => {}).catch((failure) => failure);
		expect(error.kind).toBe("fatal");
		expect(error.message).toContain(option);
		expect(fake.runs[0].toolResults).toEqual([]);
		expect(fake.runs[0].steers).toEqual([]);
		expect(fake.runs[0].interrupted).toBe(false);
		expect(writes.length).toBe(1);
		const final = await provider.step(request(history, "e1", options), 3, () => {});
		expect(JSON.parse(final.text)).toEqual({ answer: 42 });
		expect(fake.runs[0].toolResults[0].result.content[0].text).toBe("ok");
		expect(fake.runs.length).toBe(1);
	});

	test("invalid parked options preserve the active query and cannot release tools", async () => {
		const { provider, fake } = setup([{ sessionId: "cc", turns: [{ tools: [{ id: "call", name: "js", input: {} }] }, { text: "done" }] }]);
		const firstUser = user("go");
		const first = await provider.step(request([firstUser]), 1, () => {});
		const history = [firstUser, stored(first), tool("call", "ok")];
		const error = await provider.step(request(history, "e1", { output_schema: { name: "bad", schema: { type: "object" } } }), 2, () => {}).catch((failure) => failure);
		expect(error.kind).toBe("fatal");
		expect(fake.runs[0].toolResults).toEqual([]);
		expect(fake.runs[0].interrupted).toBe(false);
		expect((await provider.step(request(history), 3, () => {})).text).toBe("done");
	});

	test("resumed query maps changed effort and schema per request", async () => {
		const { provider, fake } = setup([{ sessionId: "cc", turns: [{ text: "first" }] },
			{ sessionId: "cc", turns: [{ text: "formatting" }], result: { structured_output: { answer: 42 } } }]);
		const firstUser = user("go");
		const first = await provider.step(request([firstUser]), 1, () => {});
		await provider.step(request([firstUser, stored(first), user("more")], "e1", { reasoning_effort: "max", output_schema: schema }), 2, () => {});
		expect(fake.runs[1].options.resume).toBe("cc");
		expect(fake.runs[1].options.effort).toBe("max");
		expect(fake.runs[1].options.outputFormat).toEqual({ type: "json_schema", schema: schema.schema });
	});
});

describe("SDK token usage", () => {
	const usage: FakeUsage = { input_tokens: 3, output_tokens: 8, cache_read_input_tokens: 11, cache_creation_input_tokens: 17,
		output_tokens_details: { thinking_tokens: 5 } };
	const expected = { input_tokens: 31, output_tokens: 8, cache_read_tokens: 11, cache_creation_tokens: 17, reasoning_tokens: 5 };
	for (const sessionless of [false, true]) {
		for (const nonStreaming of [false, true]) test(`${sessionless ? "sessionless" : "stateful"} ${nonStreaming ? "fallback" : "stream"} usage retains cache creation and reported thinking without double counting`, async () => {
			const { provider, steps } = setup([{ sessionId: "cc", turns: [{ text: "ok", usage, nonStreaming }] }]);
			const response = await provider.step(request([user("go")], "e1", { ...(sessionless ? { session_id: undefined } : {}) }), 1, () => {});
			expect(response.usage).toEqual(expected);
			expect(steps[0].usage).toEqual(expected);
		});
		test(`${sessionless ? "sessionless" : "stateful"} result-only usage reports genuine thinking`, async () => {
			const { provider } = setup([{ sessionId: "cc", turns: [], result: { result: "ok", usage } }]);
			expect((await provider.step(request([user("go")], "e1", { ...(sessionless ? { session_id: undefined } : {}) }), 1, () => {})).usage).toEqual(expected);
		});
		for (const thinking of [undefined, null, { thinking_tokens: 0 }]) test(`${sessionless ? "sessionless" : "stateful"} ${thinking == null ? "unknown thinking omitted" : "reported zero thinking retained"}`, async () => {
			const { provider } = setup([{ sessionId: "cc", turns: [], result: { result: "ok", usage: { ...usage, output_tokens_details: thinking } } }]);
			const response = await provider.step(request([user("go")], "e1", { ...(sessionless ? { session_id: undefined } : {}) }), 1, () => {});
			if (thinking == null) expect(Object.hasOwn(response.usage!, "reasoning_tokens")).toBe(false);
			else expect(response.usage?.reasoning_tokens).toBe(0);
		});
	}
	test("parked tool step captures final streamed thinking counters before the assistant duplicate", async () => {
		const { provider } = setup([{ sessionId: "cc", turns: [{ tools: [{ id: "call", name: "js", input: {} }], usage }, { text: "done" }] }]);
		const firstUser = user("go");
		const first = await provider.step(request([firstUser]), 1, () => {});
		expect(first.usage).toEqual(expected);
		const second = await provider.step(request([firstUser, stored(first), tool("call", "ok")]), 2, () => {});
		expect(second.usage?.input_tokens).toBe(10);
		expect(Object.hasOwn(second.usage!, "reasoning_tokens")).toBe(false);
	});
	test("one-off aggregate result replaces message totals while keeping previously reported thinking when omitted", async () => {
		const { provider, steps } = setup([{ sessionId: "cc", turns: [{ text: "ok", usage }],
			result: { usage: { input_tokens: 7, output_tokens: 12, cache_read_input_tokens: 20, cache_creation_input_tokens: 30 } } }]);
		const response = await provider.step(request([user("go")], "e1", { session_id: undefined }), 1, () => {});
		expect(response.usage).toEqual({ input_tokens: 57, output_tokens: 12, cache_read_tokens: 20, cache_creation_tokens: 30, reasoning_tokens: 5 });
		expect(steps[0].usage).toEqual(response.usage!);
	});
	test("stateful final result adds genuinely reported thinking absent from streamed message usage", async () => {
		const { provider } = setup([{ sessionId: "cc", turns: [{ text: "ok", usage: { ...usage, output_tokens_details: undefined } }], result: { usage } }]);
		expect((await provider.step(request([user("go")]), 1, () => {})).usage).toEqual(expected);
	});
	test("parked query final aggregate thinking subtracts the already returned tool step", async () => {
		const { provider } = setup([{ sessionId: "cc", turns: [
			{ tools: [{ id: "call", name: "js", input: {} }], usage },
			{ text: "done", usage: { input_tokens: 2, output_tokens: 4, cache_read_input_tokens: 1 } },
		], result: { usage: { ...usage, output_tokens_details: { thinking_tokens: 8 } } } }]);
		const firstUser = user("go");
		const first = await provider.step(request([firstUser]), 1, () => {});
		expect(first.usage?.reasoning_tokens).toBe(5);
		const second = await provider.step(request([firstUser, stored(first), tool("call", "ok")]), 2, () => {});
		expect(second.usage).toEqual({ input_tokens: 3, output_tokens: 4, cache_read_tokens: 1, cache_creation_tokens: 0, reasoning_tokens: 3 });
	});
	test("parked result-only aggregate fallback subtracts the already returned step's token usage", async () => {
		const aggregate = { input_tokens: 5, output_tokens: 12, cache_read_input_tokens: 12, cache_creation_input_tokens: 20, output_tokens_details: { thinking_tokens: 8 } };
		const { provider } = setup([{ sessionId: "cc", turns: [{ tools: [{ id: "call", name: "js", input: {} }], usage }], result: { result: "done", usage: aggregate } }]);
		const firstUser = user("go");
		const first = await provider.step(request([firstUser]), 1, () => {});
		const second = await provider.step(request([firstUser, stored(first), tool("call", "ok")]), 2, () => {});
		expect(second.usage).toEqual({ input_tokens: 6, output_tokens: 4, cache_read_tokens: 1, cache_creation_tokens: 3, reasoning_tokens: 3 });
		expect(second.text).toBe("done");
	});
});
