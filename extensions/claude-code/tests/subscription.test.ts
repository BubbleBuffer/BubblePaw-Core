// Subscription instances: per-instance env, API-billing guard, system prompt
// mode and the usage-limit reason (docs/providers.md, "Claude subscriptions").
import { describe, expect, test } from "bun:test";
import { classify, limitTypeOf } from "../src/errors.js";
import { childEnv, ClaudeCodeProvider, parseSystemPromptMode, queryOptions, type SystemPromptMode } from "../src/provider.js";
import { fakeQuery } from "./fake-sdk.js";
import { request, user } from "./helpers.js";

describe("child env", () => {
	test("an inherited API key or auth token never reaches Claude Code", () => {
		const env = childEnv({ PATH: "/bin", ANTHROPIC_API_KEY: "sk-inherited", ANTHROPIC_AUTH_TOKEN: "tok", CLAUDE_CONFIG_DIR: "/home/user/.claude-b", BUBBLEPAW_EXTENSION_ENV_KEYS: "CLAUDE_CONFIG_DIR" });
		expect(env.ANTHROPIC_API_KEY).toBeUndefined();
		expect("ANTHROPIC_API_KEY" in env).toBe(false);
		expect("ANTHROPIC_AUTH_TOKEN" in env).toBe(false);
		expect(env.CLAUDE_CONFIG_DIR).toBe("/home/user/.claude-b");
		expect(env.PATH).toBe("/bin");
		expect(env.ENABLE_CLAUDEAI_MCP_SERVERS).toBe("0");
		expect(env.DISABLE_AUTO_COMPACT).toBe("1");
	});

	test("without the host's key list the API key is still dropped", () => {
		expect("ANTHROPIC_API_KEY" in childEnv({ ANTHROPIC_API_KEY: "sk-inherited" })).toBe(false);
	});

	test("an API key the instance env sets explicitly is kept", () => {
		const env = childEnv({ ANTHROPIC_API_KEY: "sk-explicit", BUBBLEPAW_EXTENSION_ENV_KEYS: "CLAUDE_CONFIG_DIR,ANTHROPIC_API_KEY" });
		expect(env.ANTHROPIC_API_KEY).toBe("sk-explicit");
	});

	test("every step query gets the filtered env", async () => {
		const fake = fakeQuery([{ sessionId: "cc1", turns: [{ text: "hi" }] }]);
		const provider = new ClaudeCodeProvider({
			query: fake.fn,
			cwd: ".",
			env: { CLAUDE_CONFIG_DIR: "/home/user/.claude-b", ANTHROPIC_API_KEY: "sk-inherited", BUBBLEPAW_EXTENSION_ENV_KEYS: "CLAUDE_CONFIG_DIR" },
			writeSession: () => ({ sessionId: null, records: 0, warnings: [] }),
		});
		await provider.step(request([user("go")]), 1, () => {});
		const env = fake.runs[0].options.env;
		expect(env.CLAUDE_CONFIG_DIR).toBe("/home/user/.claude-b");
		expect("ANTHROPIC_API_KEY" in env).toBe(false);
	});
});

describe("system prompt mode", () => {
	const base = { cwd: ".", model: "haiku", systemPrompt: "You are BubblePaw." };

	test("preset-append is the default", () => {
		expect(parseSystemPromptMode(undefined)).toBe("preset-append");
		expect(parseSystemPromptMode("")).toBe("preset-append");
		expect(queryOptions(base).systemPrompt).toEqual({ type: "preset", preset: "claude_code", append: "You are BubblePaw." });
	});

	test("replace sends BubblePaw's prompt alone as a plain string", () => {
		expect(parseSystemPromptMode("replace")).toBe("replace");
		expect(queryOptions({ ...base, systemPromptMode: "replace" }).systemPrompt).toBe("You are BubblePaw.");
	});

	test("an unknown mode is refused", () => {
		expect(() => parseSystemPromptMode("append")).toThrow(/preset-append or replace/);
	});

	for (const [mode, expected] of [
		["replace", "You are BubblePaw."],
		["preset-append", { type: "preset", preset: "claude_code", append: "You are BubblePaw." }],
	] as [SystemPromptMode, unknown][]) {
		test(`a session step uses ${mode}`, async () => {
			const fake = fakeQuery([{ sessionId: "cc1", turns: [{ text: "hi" }] }]);
			const provider = new ClaudeCodeProvider({
				query: fake.fn,
				cwd: ".",
				systemPromptMode: mode,
				writeSession: () => ({ sessionId: null, records: 0, warnings: [] }),
			});
			await provider.step(request([user("go")]), 1, () => {});
			expect(fake.runs[0].options.systemPrompt).toEqual(expected);
		});
	}
});

describe("usage-limit reason", () => {
	test("a subscription rejection keeps its limit type and reset time", () => {
		const error = classify("You've hit your limit", { rateLimitRejection: { rateLimitType: "five_hour", resetsAt: 1791468000 } });
		expect(error.kind).toBe("rate_limit");
		expect(error.data).toMatchObject({ kind: "rate_limit", resets_at: "2026-10-08T14:00:00Z", limit_type: "five_hour" });
	});

	test("an unnamed or odd limit type becomes `subscription`", () => {
		expect(limitTypeOf({})).toBe("subscription");
		expect(limitTypeOf({ rateLimitType: "Five Hours; rm -rf" })).toBe("subscription");
		expect(limitTypeOf(null)).toBeUndefined();
	});

	test("a plain 429 without a rejection event has no limit type", () => {
		expect(classify("HTTP 429 too many requests").data).toEqual({ kind: "rate_limit" });
		expect(classify("x", { assistantError: "rate_limit" }).data).toEqual({ kind: "rate_limit" });
	});

	test("the provider reports a five_hour cap from the rate_limit_event", async () => {
		const fake = fakeQuery([{ sessionId: "cc1", turns: [], error: { result: "You've hit your limit · resets 6pm", rateLimit: { rateLimitType: "five_hour", resetsAt: 1791468000 }, assistantError: "rate_limit" } }]);
		const provider = new ClaudeCodeProvider({ query: fake.fn, cwd: ".", writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
		const error = await provider.step(request([user("go")]), 1, () => {}).catch((e) => e);
		expect(error.data).toMatchObject({ kind: "rate_limit", resets_at: "2026-10-08T14:00:00Z", limit_type: "five_hour" });
		expect(error.message).toBe("Claude Code rate limit exceeded");
	});
});
