import { afterEach, describe, expect, test } from "bun:test";
import { join } from "node:path";
import { resolveModel } from "../src/models.js";
import { ProviderError } from "../src/types.js";
import { assistant, fakeProvider, LOOKUP, request, stored, tool, user } from "./helpers.js";

const live: Array<{ provider: { shutdown(): void } }> = [];
function setup(settleMs?: number) {
	const f = fakeProvider(settleMs);
	live.push(f);
	return f;
}
afterEach(() => {
	for (const f of live.splice(0)) f.provider.shutdown();
});

describe("step and park/deliver", () => {
	test("a tool call parks and returns; the next step's result continues the same agy turn", async () => {
		const { provider, calls, received } = setup();
		const texts: string[] = [];
		const first = await provider.step(request([user('CALL lookup {"name":"a"}')]), 1, (t) => texts.push(t));
		expect(first.tool_calls).toHaveLength(1);
		expect(first.tool_calls[0].name).toBe("lookup");
		expect(first.tool_calls[0].arguments).toEqual({ name: "a" });
		expect(first.text).toBe("calling. ");
		expect(texts.join("")).toBe("calling. ");
		expect(first.usage).toBeNull(); // the turn's usage arrives with its final step
		// agy was told about the bubblepaw MCP server, the model, and the tool-only note
		const created = calls("session/new")[0].params;
		expect(created.mcpServers[0].name).toBe("bubblepaw");
		expect(created._meta.antigravity.systemPrompt.append).toContain("You are BubblePaw.");
		expect(created._meta.antigravity.systemPrompt.append).toContain("mcp_bubblepaw_");
		expect(calls("session/set_model")[0].params.modelId).toBe("gemini-3.8-flash-low");
		expect(received().find((r) => r.shimTools)?.shimTools).toEqual(["lookup"]);

		const messages = [user('CALL lookup {"name":"a"}'), stored(first), tool(first.tool_calls[0].id, "4417")];
		const second = await provider.step(request(messages), 2, () => {});
		expect(second.tool_calls).toHaveLength(0);
		expect(second.text).toBe("got: 4417");
		expect(second.usage).toEqual({ input_tokens: 100, output_tokens: 7, cache_read_tokens: 40, cache_creation_tokens: 0, reasoning_tokens: 3 });
		expect(second.reported_model).toBe("gemini-3.8-flash");
		expect(calls("session/new")).toHaveLength(1);
		expect(calls("session/prompt")).toHaveLength(1);
	});
});

describe("context ownership", () => {
	test("an append-only request reuses the agy session and sends only the new input", async () => {
		const { provider, calls } = setup();
		const m1 = user("hello");
		const first = await provider.step(request([m1]), 1, () => {});
		await provider.step(request([m1, stored(first), user("and then?")]), 2, () => {});
		expect(calls("session/new")).toHaveLength(1);
		const prompts = calls("session/prompt");
		expect(prompts).toHaveLength(2);
		expect(prompts[1].params.prompt[0].text).toBe("and then?");
	});

	test("a non-append request (new epoch) starts a new agy session seeded with the history", async () => {
		const { provider, calls } = setup();
		const m1 = user("my favourite colour is teal");
		const first = await provider.step(request([m1]), 1, () => {});
		const rewritten = [user("Summary: the user's favourite colour is teal."), user("what colour do I like?")];
		await provider.step(request([m1, stored(first), ...rewritten], "e2"), 2, () => {});
		expect(calls("session/new")).toHaveLength(2);
		const seed: string = calls("session/prompt")[1].params.prompt[0].text;
		expect(seed).toContain("<conversation_so_far>");
		expect(seed).toContain("user: my favourite colour is teal");
		expect(seed).toContain("assistant: echo");
		expect(seed.trimEnd().endsWith("Current user input:\nSummary: the user's favourite colour is teal.\n\nwhat colour do I like?")).toBe(true);
		expect(calls("session/close")).toHaveLength(1); // the superseded session
	});
});

describe("deny built-in tools", () => {
	test("permission requests: bubblepaw MCP tools are allowed, anything else is rejected", async () => {
		const { provider } = setup();
		const reply = await provider.step(request([user("BUILTIN")]), 1, () => {});
		expect(reply.text).toBe("builtin=deny mcp=allow");
	});

	test("the PreToolUse hook denies everything but bubblepaw tools", async () => {
		const run = async (name: string) => {
			const proc = Bun.spawn([process.execPath, join(import.meta.dir, "../src/hook.ts")], { stdin: new Blob([JSON.stringify({ hookEventName: "PreToolUse", toolCall: { name } })]), stdout: "pipe" });
			return JSON.parse(await new Response(proc.stdout).text());
		};
		expect(await run("run_command")).toEqual({ decision: "deny", reason: "Use the bubblepaw tools; built-in tools are disabled here." });
		expect(await run("mcp_other_thing")).toMatchObject({ decision: "deny" });
		expect(await run("mcp_bubblepaw_lookup")).toEqual({ decision: "allow" });
	});
});

describe("cancel and interruption", () => {
	test("a turn that ended while its call was parked cannot take the result: the next step reseeds with the result in the seed", async () => {
		const { provider, calls } = setup(100); // the step returns before the fake gives up after 300 ms
		const first = await provider.step(request([user('TIMEOUT lookup {"name":"a"}')]), 1, () => {});
		await Bun.sleep(500); // by now the turn has ended
		const messages = [user('TIMEOUT lookup {"name":"a"}'), stored(first), tool(first.tool_calls[0].id, "4417")];
		const second = await provider.step(request(messages), 2, () => {});
		expect(first.tool_calls).toHaveLength(1);
		expect(calls("session/new")).toHaveLength(2);
		const seed: string = calls("session/prompt")[1].params.prompt[0].text;
		expect(seed).toContain("tool result");
		expect(seed).toContain("4417");
		expect(second.text).toBe("echo");
	});

	test("cancel releases the parked call, cancels agy's turn and fails the step as cancelled", async () => {
		const { provider, calls, received } = setup(5000); // a long settle window keeps the step waiting with the call parked
		const running = provider.step(request([user('CALL lookup {"name":"a"}')]), 7, () => {});
		for (let i = 0; i < 100 && !received().some((r) => r.shimTools); i++) await Bun.sleep(100); // the shim is up
		await Bun.sleep(300); // the tools/call is parked
		provider.cancel(7);
		await expect(running).rejects.toMatchObject({ kind: "transient", message: "cancelled" });
		await Bun.sleep(200);
		expect(calls("session/cancel")).toHaveLength(1);
		expect(received().find((r) => r.toolResult)?.toolResult).toMatchObject({ isError: true, content: [{ text: "cancelled" }] });
		// the next step starts a fresh seeded session; the parked call is never replayed
		await provider.step(request([user("again")], "e1"), 8, () => {});
		expect(calls("session/new")).toHaveLength(2);
	});

	test("agy dying mid-turn is a transient error and the next step starts a fresh process and session", async () => {
		const { provider, calls } = setup();
		await expect(provider.step(request([user("DIE")]), 1, () => {})).rejects.toBeInstanceOf(ProviderError);
		const reply = await provider.step(request([user("hello")]), 2, () => {});
		expect(reply.text).toBe("echo");
		expect(calls("session/new").length).toBeGreaterThanOrEqual(1);
	});
});

describe("model mapping", () => {
	const available = ["m-low", "m-medium", "m-high"];
	test("effort picks the suffix; a missing level steps down; a full id passes through", () => {
		expect(resolveModel(available, "m", null)).toEqual({ id: "m-low", base: "m" });
		expect(resolveModel(available, "m", "medium")).toEqual({ id: "m-medium", base: "m" });
		expect(resolveModel(available, "m", "max")).toEqual({ id: "m-high", base: "m" });
		expect(resolveModel(available, "m-high", "low")).toEqual({ id: "m-high", base: "m" });
		expect(resolveModel(available, "unknown", "high")).toEqual({ id: "unknown", base: "unknown" });
	});
});
