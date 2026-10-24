import { describe, expect, test } from "bun:test";
import { classify } from "../src/errors.js";
import { ClaudeCodeProvider } from "../src/provider.js";
import { rateLimitInfo } from "../src/quota.js";
import { fakeQuery } from "./fake-sdk.js";
import { request, user } from "./helpers.js";

describe("quota events", () => {
	const info = { status: "allowed_warning", rateLimitType: "five_hour", utilization: 0.72, resetsAt: 1791468000, overageStatus: "allowed", overageResetsAt: 1791469000, isUsingOverage: false, surpassedThreshold: 0.7 } as const;
	for (const oneOff of [false, true]) test(`successful ${oneOff ? "one-off" : "session"} step forwards meter events`, async () => {
		const fake = fakeQuery([{ sessionId: "cc1", turns: [{ text: "hi" }], rateLimits: [info] }]);
		const provider = new ClaudeCodeProvider({ query: fake.fn, cwd: ".", writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
		const req = request([user("go")]);
		if (oneOff) delete req.session_id;
		const response = await provider.step(req, 1, () => {});
		expect(response.quota_events).toEqual([info]);
	});

	test("failed step retains warning and rejection evidence", async () => {
		const fake = fakeQuery([{ sessionId: "cc1", turns: [], rateLimits: [info], error: { result: "limit reached", rateLimit: { rateLimitType: "seven_day", resetsAt: 1791468000 } } }]);
		const provider = new ClaudeCodeProvider({ query: fake.fn, cwd: ".", writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
		const error = await provider.step(request([user("go")]), 1, () => {}).catch(error => error);
		expect(error.kind).toBe("rate_limit");
		expect(error.data.quota_events).toEqual([info, { status: "rejected", rateLimitType: "seven_day", resetsAt: 1791468000 }]);
	});

	test("only typed quota fields survive", () => {
		expect(rateLimitInfo({ ...info, utilization: Number.NaN, token: "secret", rateLimitType: "bad space", resetsAt: 1e300, arbitrary: "response text" })).toEqual({ status: "allowed_warning", overageStatus: "allowed", overageResetsAt: 1791469000, isUsingOverage: false, surpassedThreshold: 0.7 });
		expect(rateLimitInfo({ status: "not-a-status" })).toBeUndefined();
	});

	test("all supplied unified buckets survive below warning threshold", () => {
		const value = { status: "allowed", unifiedWindows: { five_hour: { utilization: 0.24, resetsAt: 1791468000 }, seven_day: { utilization: 0.13, resetsAt: 1791469000 } } } as const;
		expect(rateLimitInfo(value)).toEqual(value);
	});

	for (const failure of ["enforced_spend_limit_reached", "billing_error", "insufficient_credits", "credits_required", "error_max_budget_usd"]) test(`${failure} is terminal`, () => {
		expect(classify(`HTTP 429 ${failure}`).kind).toBe("fatal");
		expect(classify(`HTTP 429 ${failure}`).data.terminal_quota).toBe(true);
	});
	test("credits-required SDK rejection is terminal", () => {
		const error = classify("limit reached", { rateLimitRejection: { status: "rejected", errorCode: "credits_required" } });
		expect(error.kind).toBe("fatal");
		expect(error.data.terminal_quota).toBe(true);
	});
	test("SDK budget result remains terminal when its errors omit the code", async () => {
		const fake = fakeQuery([{ sessionId: "cc1", turns: [], error: { result: "budget spent", subtype: "error_max_budget_usd", errors: ["budget spent"] } }]);
		const provider = new ClaudeCodeProvider({ query: fake.fn, cwd: ".", writeSession: () => ({ sessionId: null, records: 0, warnings: [] }) });
		const error = await provider.step(request([user("go")]), 1, () => {}).catch(error => error);
		expect(error.kind).toBe("fatal"); expect(error.data.terminal_quota).toBe(true);
	});
});
