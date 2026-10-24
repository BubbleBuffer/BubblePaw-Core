import { describe, expect, test } from "bun:test";
import { cancelled, classify, classifyThrown } from "../src/errors.js";
import { handlers } from "../src/main.js";
import { ClaudeCodeProvider, type StepLog } from "../src/provider.js";
import { encode, errorRecord } from "../src/rpc.js";
import { ProviderError } from "../src/types.js";
import { fakeQuery } from "./fake-sdk.js";
import { request, user } from "./helpers.js";

const sentinel = "SENTINEL_PRIVATE_PROVIDER_PAYLOAD";
const fixed = {
	auth: "Claude Code authentication failed",
	context: "Claude Code context limit exceeded",
	rate_limit: "Claude Code rate limit exceeded",
	transient: "Claude Code transport failed",
	fatal: "Claude Code request failed",
};

describe("safe SDK error boundary", () => {
	for (const [raw, kind] of [
		["HTTP 401 unauthorized", "auth"], ["HTTP 403 forbidden", "auth"],
		["Prompt is too long", "context"], ["HTTP 429 too many requests", "rate_limit"],
		["HTTP 500 server error", "transient"], ["HTTP 501", "transient"], ["HTTP 502", "transient"], ["HTTP 503", "transient"],
		["HTTP 504", "transient"], ["HTTP 529 overloaded", "transient"], ["ECONNRESET", "transient"],
		["fetch failed", "transient"], ["EPIPE", "transient"], ["unrecognized SDK failure", "fatal"],
	] as const) test(`${raw} classification keeps its kind and emits fixed safe messages`, () => {
		const input = `${raw}: ${sentinel}`;
		for (const error of [classify(input), classifyThrown(new Error(input)), classifyThrown(input)]) {
			expect(error.kind).toBe(kind);
			expect(error.message).toBe(fixed[kind]);
			expect(encode(errorRecord(1, error))).not.toContain(sentinel);
		}
		expect(errorRecord(1, new Error(input)).error.message).toBe(fixed[kind]);
	});

	test("rate-limit resets_at survives safe classification without raw provider text", () => {
		const error = classify(sentinel, { rateLimitRejection: { resetsAt: 1791468000 } });
		expect(error.data).toMatchObject({ kind: "rate_limit", resets_at: "2026-10-08T14:00:00Z", limit_type: "subscription" });
		expect(error.message).toBe(fixed.rate_limit);
	});

	test("trusted internal validation and cancellation ProviderErrors preserve actionable messages", () => {
		const internal = new ProviderError("fatal", "output_schema cannot change while Claude Code is waiting for tool results");
		expect(classifyThrown(internal)).toBe(internal);
		expect(errorRecord(1, internal).error.message).toBe(internal.message);
		expect(classifyThrown(cancelled()).message).toBe("cancelled");
	});

	for (const sessionless of [false, true]) {
		test(`${sessionless ? "sessionless" : "stateful"} synchronous SDK startup failure is typed and safe with one query attempt`, async () => {
			let attempts = 0;
			const steps: StepLog[] = [];
			const provider = new ClaudeCodeProvider({ query: () => { attempts++; throw new Error(`HTTP 403: ${sentinel}`); },
				onStep: (step) => steps.push(step), writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
			const h = handlers(provider, () => {});
			const error = await h.request("step", request([user("go")], "e1", { ...(sessionless ? { session_id: undefined } : {}) }), 1).catch((failure) => failure);
			expect(error).toBeInstanceOf(ProviderError);
			if (!(error instanceof ProviderError)) throw new Error("expected ProviderError from SDK startup");
			expect(error.kind).toBe("auth");
			expect(error.message).toBe(fixed.auth);
			expect(attempts).toBe(1);
			expect(encode(errorRecord(1, error))).not.toContain(sentinel);
			expect(JSON.stringify(steps)).not.toContain(sentinel);
		});

		test(`${sessionless ? "sessionless" : "stateful"} result errors and synthetic error text cannot escape typed replies or step logs`, async () => {
			const fake = fakeQuery([{ sessionId: "cc", turns: [], error: { result: `HTTP 401 ${sentinel}`, assistantError: "authentication_failed", subtype: "error_during_execution", errors: [`HTTP 401 ${sentinel}`] } }]);
			const steps: StepLog[] = [];
			const provider = new ClaudeCodeProvider({ query: fake.fn, onStep: (step) => steps.push(step), writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
			const deltas: string[] = [];
			const error = await provider.step(request([user("go")], "e1", { ...(sessionless ? { session_id: undefined } : {}) }), 1, (text) => deltas.push(text)).catch((failure) => failure);
			expect(error.kind).toBe("auth");
			expect(error.message).toBe(fixed.auth);
			expect(deltas).toEqual([]);
			expect(encode(errorRecord(1, error))).not.toContain(sentinel);
			expect(JSON.stringify(steps)).not.toContain(sentinel);
			expect(fake.runs.length).toBe(1);
		});

		test(`${sessionless ? "sessionless" : "stateful"} midstream transport failure after published text is safe and extension does not retry`, async () => {
			const fake = fakeQuery([{ sessionId: "cc", turns: [{ text: "published" }], throwAfterTurns: `ECONNRESET ${sentinel}` }]);
			const steps: StepLog[] = [];
			const logs: string[] = [];
			const provider = new ClaudeCodeProvider({ query: fake.fn, onStep: (step) => steps.push(step), log: (line) => logs.push(line), writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
			const deltas: string[] = [];
			const error = await provider.step(request([user("go")], "e1", { ...(sessionless ? { session_id: undefined } : {}) }), 1, (text) => deltas.push(text)).catch((failure) => failure);
			expect(deltas.join("")).toBe("published");
			expect(error.kind).toBe("transient");
			expect(error.message).toBe(fixed.transient);
			expect(fake.runs.length).toBe(1);
			fake.runs[0].options.stderr(`diagnostic: ${sentinel}`);
			expect(logs).toContain("claude: process diagnostic received");
			expect(encode(errorRecord(1, error))).not.toContain(sentinel);
			expect(JSON.stringify(steps)).not.toContain(sentinel);
			expect(JSON.stringify(logs)).not.toContain(sentinel);
		});
	}
});
