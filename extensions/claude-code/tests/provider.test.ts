import { describe, expect, test } from "bun:test";
import { classify, classifyThrown, resetsAtIso } from "../src/errors.js";
import { ParkingLot } from "../src/mcp-server.js";
import { ClaudeCodeProvider, type StepLog } from "../src/provider.js";
import { ProviderError } from "../src/types.js";
import { fakeQuery } from "./fake-sdk.js";
import { note, request, stored, tool, user } from "./helpers.js";

function setup(scripts: Parameters<typeof fakeQuery>[0]) {
	const fake = fakeQuery(scripts);
	const steps: StepLog[] = [];
	const writes: any[] = [];
	const provider = new ClaudeCodeProvider({
		query: fake.fn,
		cwd: ".",
		onStep: (s) => steps.push(s),
		writeSession: (opts) => {
			writes.push(opts);
			return { sessionId: opts.priors.length ? `rebuilt-${writes.length}` : null, records: opts.priors.length, warnings: [] };
		},
	});
	return { provider, fake, steps, writes };
}

describe("parking", () => {
	test("a result delivered before its call parks is queued; releaseAll answers parked calls", async () => {
		const lot = new ParkingLot();
		expect(lot.deliver("a", { content: [{ type: "text", text: "A" }] })).toBe("queued");
		expect(await lot.park("a", "js")).toEqual({ content: [{ type: "text", text: "A" }] });
		const waiting = lot.park("b", "js");
		expect(lot.deliver("b", { content: [{ type: "text", text: "B" }] })).toBe("released");
		expect((await waiting).content[0].text).toBe("B");
		const stranded = lot.park("c", "js");
		lot.releaseAll("gone");
		expect(await stranded).toEqual({ content: [{ type: "text", text: "gone" }], isError: true });
	});
});

describe("step engine with a fake Claude Code", () => {
	test("two tool calls in one turn are returned together, released by the next step, then reuse", async () => {
		const { provider, fake, steps } = setup([
			{
				sessionId: "cc1",
				turns: [
					{ text: "Let me look.", tools: [{ id: "toolu_1", name: "js", input: { code: "a" } }, { id: "toolu_2", name: "js", input: { code: "b" } }], usage: { input_tokens: 3, cache_read_input_tokens: 0, cache_creation_input_tokens: 900, output_tokens: 20 } },
					{ text: "The answer is 42.", usage: { input_tokens: 4, cache_read_input_tokens: 900, cache_creation_input_tokens: 50, output_tokens: 8 } },
				],
			},
			{ sessionId: "cc1", turns: [{ text: "Again: 42.", usage: { input_tokens: 5, cache_read_input_tokens: 950, output_tokens: 4 } }] },
		]);
		const texts: string[] = [];
		const u = user("compute");
		const first = await provider.step(request([u]), 1, (t) => texts.push(t));
		expect(first.text).toBe("Let me look.");
		expect(first.tool_calls).toEqual([
			{ id: "toolu_1", name: "js", arguments: { code: "a" } },
			{ id: "toolu_2", name: "js", arguments: { code: "b" } },
		]);
		expect(first.usage).toEqual({ input_tokens: 903, output_tokens: 20, cache_read_tokens: 0, cache_creation_tokens: 900 });
		expect(first.provider_state).toMatchObject({ claude_code: { session_id: "cc1", tool_use_ids: ["toolu_1", "toolu_2"], message_id: "msg_cc1_0" } });
		expect(texts.join("")).toBe("Let me look.");
		const run = fake.runs[0];
		expect(run.prompt).toBe("compute");
		expect(run.options.tools).toEqual([]);
		expect(run.options.settingSources).toEqual([]);
		expect(run.options.strictMcpConfig).toBe(true);
		expect(run.options.env.ENABLE_CLAUDEAI_MCP_SERVERS).toBe("0");
		expect(run.options.env.DISABLE_AUTO_COMPACT).toBe("1");
		expect(run.options.systemPrompt).toEqual({ type: "preset", preset: "claude_code", append: "You are BubblePaw." });
		expect(run.options.resume).toBeUndefined();

		const a1 = stored(first);
		const second = await provider.step(request([u, a1, tool("toolu_2", "B"), tool("toolu_1", "A")]), 2, () => {});
		expect(second.text).toBe("The answer is 42.");
		expect(second.tool_calls).toEqual([]);
		expect(second.usage).toEqual({ input_tokens: 954, output_tokens: 8, cache_read_tokens: 900, cache_creation_tokens: 50 });
		expect(run.toolResults.map((r) => [r.id, r.result.content[0].text])).toEqual([["toolu_1", "A"], ["toolu_2", "B"]]);
		expect(fake.runs.length).toBe(1);

		const third = await provider.step(request([u, a1, tool("toolu_2", "B"), tool("toolu_1", "A"), stored(second), user("again")]), 3, () => {});
		expect(third.text).toBe("Again: 42.");
		expect(fake.runs[1].options.resume).toBe("cc1");
		expect(fake.runs[1].prompt).toBe("again");
		expect(steps.map((s) => s.path)).toEqual(["fresh", "deliver", "reuse"]);
		expect(steps[2].usage?.cache_read_tokens).toBe(950);
	});

	test("a new epoch rebuilds and kills a parked query", async () => {
		const { provider, fake, steps, writes } = setup([
			{ sessionId: "cc1", turns: [{ tools: [{ id: "toolu_1", name: "js", input: {} }] }, { text: "never" }] },
			{ sessionId: "rebuilt-2", turns: [{ text: "continued" }] },
		]);
		const u = user("go");
		const first = await provider.step(request([u]), 1, () => {});
		const second = await provider.step(request([u, stored(first), tool("toolu_1", "r")], "e2"), 2, () => {});
		expect(second.text).toBe("continued");
		expect(fake.runs[0].interrupted).toBe(true);
		expect(writes[1].forceRotate).toBe(true);
		expect(writes[1].priors.length).toBe(3);
		expect(fake.runs[1].options.resume).toBe("rebuilt-2");
		expect(fake.runs[1].prompt).toContain("Continue the task");
		expect(steps.map((s) => [s.path, s.reason])).toEqual([["fresh", "no mirror"], ["rebuild", "new epoch"]]);
	});

	test("non-streaming fallback message still returns its tool call", async () => {
		const { provider } = setup([{ sessionId: "cc1", turns: [{ nonStreaming: true, text: "x", tools: [{ id: "toolu_9", name: "js", input: { code: "1" } }] }, { text: "ok" }] }]);
		const r = await provider.step(request([user("go")]), 1, () => {});
		expect(r.tool_calls.map((c) => c.id)).toEqual(["toolu_9"]);
	});

	test("cancel interrupts the query and fails transient/cancelled; the next step rebuilds", async () => {
		const { provider, fake, writes } = setup([
			{ sessionId: "cc1", turns: [{ text: "hi" }] },
			{ sessionId: "cc1", turns: [], hang: true },
			{ sessionId: "rebuilt-2", turns: [{ text: "after" }] },
		]);
		const u = user("go");
		const first = await provider.step(request([u]), 1, () => {});
		const history = [u, stored(first), user("more")];
		const pending = provider.step(request(history), 2, () => {});
		await Bun.sleep(20);
		provider.cancel(2);
		const error = await pending.catch((e) => e);
		expect(error).toBeInstanceOf(ProviderError);
		// The SDK reported input before the cancel; a cancelled call is still metered.
		expect(error.data).toEqual({ kind: "transient", usage: { input_tokens: 1, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0 } });
		expect(error.message).toBe("cancelled");
		expect(fake.runs[1].interrupted).toBe(true);
		const third = await provider.step(request([...history, user("retry")]), 3, () => {});
		expect(third.text).toBe("after");
		expect(writes.at(-1).forceRotate).toBe(true);
	});

	test("a usage limit fails as rate_limit with resets_at", async () => {
		const { provider } = setup([{ sessionId: "cc1", turns: [], error: { result: "You're out of extra usage · resets 6:30pm", rateLimit: { resetsAt: 1791468000 }, assistantError: "rate_limit" } }]);
		const error = await provider.step(request([user("go")]), 1, () => {}).catch((e) => e);
		expect(error.data).toMatchObject({ kind: "rate_limit", resets_at: "2026-10-08T14:00:00Z", limit_type: "subscription" });
	});

	test("session.close ends the query and forgets the mirror", async () => {
		const { provider, fake } = setup([{ sessionId: "cc1", turns: [{ tools: [{ id: "toolu_1", name: "js", input: {} }] }, { text: "x" }] }]);
		await provider.step(request([user("go")]), 1, () => {});
		expect(provider.mirrors.has("s")).toBe(true);
		provider.closeSession("s");
		expect(provider.mirrors.has("s")).toBe(false);
		expect(provider.lives.has("s")).toBe(false);
		expect(fake.runs[0].interrupted).toBe(true);
	});
});

describe("ephemeral messages and steering", () => {
	test("a changing runtime note keeps every step on the reuse paths and reaches Claude Code", async () => {
		const { provider, fake, steps } = setup([
			{ sessionId: "cc1", turns: [{ tools: [{ id: "toolu_1", name: "js", input: { code: "a" } }] }, { text: "done" }] },
			{ sessionId: "cc1", turns: [{ text: "again" }] },
		]);
		const u = user("go");
		const first = await provider.step(request([u, note("jobs: 0")]), 1, () => {});
		expect(fake.runs[0].prompt).toBe("go\n\n<system-note>\njobs: 0\n</system-note>");
		const a1 = stored(first);
		const r1 = tool("toolu_1", "A");
		const second = await provider.step(request([u, a1, r1, note("jobs: 1")]), 2, () => {});
		expect(second.text).toBe("done");
		expect(fake.runs[0].toolResults[0].result.content.map((b: any) => b.text)).toEqual(["A", "<system-note>\njobs: 1\n</system-note>"]);
		expect(provider.mirrors.get("s")?.cursor).toBe(3);
		const third = await provider.step(request([u, a1, r1, stored(second), user("more"), note("jobs: 2")]), 3, () => {});
		expect(third.text).toBe("again");
		expect(fake.runs[1].prompt).toBe("<system-note>\njobs: 2\n</system-note>\n\nmore");
		expect(steps.map((s) => s.path)).toEqual(["fresh", "deliver", "reuse"]);
	});

	test("a user message arriving with the tool results is steered into the running query", async () => {
		const { provider, fake, steps } = setup([
			{ sessionId: "cc1", turns: [{ tools: [{ id: "toolu_1", name: "js", input: { code: "sleep" } }] }, { text: "Done, and B too." }] },
			{ sessionId: "cc1", turns: [{ text: "next answer" }] },
		]);
		const u = user("go");
		const first = await provider.step(request([u]), 1, () => {});
		const steer = user("also do B");
		const history = [u, stored(first), tool("toolu_1", "slept"), steer, note("n")];
		const second = await provider.step(request(history), 2, () => {});
		expect(second.text).toBe("Done, and B too.");
		const run = fake.runs[0];
		expect(run.steers).toEqual([{ text: "also do B", priority: "next" }]);
		// Written to stdin before the result is released, so it lands at this tool boundary.
		expect(run.events).toEqual(["steer:also do B", "result:toolu_1"]);
		expect(second.provider_state).toMatchObject({ claude_code: { session_id: "cc1", tool_use_ids: [], steered: [steer.id] } });
		expect(steps[1]).toMatchObject({ path: "deliver", reason: "steered 1", steered: 1 });
		expect(fake.runs.length).toBe(1);
		const third = await provider.step(request([...history.slice(0, 4), stored(second), user("next")]), 3, () => {});
		expect(third.text).toBe("next answer");
		expect(steps.map((s) => s.path)).toEqual(["fresh", "deliver", "reuse"]);
	});
});

describe("error-kind mapping", () => {
	test("kinds", () => {
		expect(classify("Not logged in · Please run /login").kind).toBe("auth");
		expect(classify("whatever", { assistantError: "authentication_failed" }).kind).toBe("auth");
		expect(classify("Prompt is too long").kind).toBe("context");
		expect(classify("Claude AI usage limit reached|1791468000").kind).toBe("rate_limit");
		expect(classify("API Error: 529 Overloaded").kind).toBe("transient");
		expect(classifyThrown(new Error("Claude Code process exited with code 1")).kind).toBe("transient");
		expect(classifyThrown(new Error("fetch failed")).kind).toBe("transient");
		expect(classify("Unknown tool schema problem").kind).toBe("fatal");
		expect(classify("x", { rateLimitRejection: { resetsAt: 1791468000 } }).data).toEqual({ kind: "rate_limit", resets_at: "2026-10-08T14:00:00Z", limit_type: "subscription" });
		expect(resetsAtIso(undefined)).toBeUndefined();
	});
});
