// Usage attribution: the full model ID the SDK reports names the usage row, never
// the configured alias (`haiku`). Covers persistent steps (tool and final), one-off
// summaries and failed calls that reported usage.
import { describe, expect, test } from "bun:test";
import { dominantModel, reportedModelId } from "../src/models.js";
import { ClaudeCodeProvider, type StepLog } from "../src/provider.js";
import { ProviderError } from "../src/types.js";
import { fakeQuery, type FakeScript, type FakeUsage } from "./fake-sdk.js";
import { request, stored, tool, user } from "./helpers.js";

const HAIKU = "claude-haiku-5-5";

function setup(scripts: FakeScript[]) {
	const fake = fakeQuery(scripts);
	const steps: StepLog[] = [];
	const provider = new ClaudeCodeProvider({
		query: fake.fn,
		cwd: ".",
		onStep: (step) => steps.push(step),
		writeSession: () => ({ sessionId: null, records: 0, warnings: [] }),
	});
	return { provider, steps };
}

// Cache creation and reads are part of input; the totals below follow that rule.
const usage: FakeUsage = { input_tokens: 10, cache_read_input_tokens: 4, cache_creation_input_tokens: 2, output_tokens: 6 };
const totals = { input_tokens: 16, output_tokens: 6, cache_read_tokens: 4, cache_creation_tokens: 2 };
const sessionless = (messages = [user("summarize")]) => ({ ...request(messages), session_id: undefined });

describe("reported model IDs", () => {
	test("configured aliases and synthetic markers are never reported IDs", () => {
		expect(reportedModelId("haiku")).toBeUndefined();
		expect(reportedModelId("opus[1m]")).toBeUndefined();
		expect(reportedModelId("<synthetic>")).toBeUndefined();
		expect(reportedModelId("")).toBeUndefined();
		expect(reportedModelId(undefined)).toBeUndefined();
		expect(reportedModelId(HAIKU)).toBe(HAIKU);
	});

	test("several models in one step name the one with most output", () => {
		expect(dominantModel([["claude-haiku-5-5", 3], ["claude-opus-5-5", 40]])).toBe("claude-opus-5-5");
		expect(dominantModel([["claude-haiku-5-5", 5], ["claude-opus-5-5", 2], ["claude-haiku-5-5", 1]])).toBe(HAIKU);
		expect(dominantModel([["haiku", 9], [undefined, 9]])).toBeUndefined();
	});
});

describe("persistent steps", () => {
	test("tool and final steps report the SDK's full ID while the request keeps the alias", async () => {
		const { provider, steps } = setup([{ sessionId: "cc", turns: [
			{ model: HAIKU, tools: [{ id: "call", name: "js", input: { code: "a" } }], usage },
			{ model: HAIKU, text: "done", usage },
		] }]);
		const firstUser = user("go");
		const first = await provider.step(request([firstUser]), 1, () => {});
		expect(first.reported_model).toBe(HAIKU);
		expect(steps[0].reported_model).toBe(HAIKU);
		const second = await provider.step(request([firstUser, stored(first), tool("call", "ok")]), 2, () => {});
		expect(second.reported_model).toBe(HAIKU);
		expect(steps[1].reported_model).toBe(HAIKU);
		expect(second.text).toBe("done");
	});

	test("a final step with only result totals names the SDK's modelUsage key", async () => {
		const { provider, steps } = setup([{ sessionId: "cc", turns: [], result: { result: "ok", usage: {
			input_tokens: 10, cache_read_input_tokens: 4, cache_creation_input_tokens: 2, output_tokens: 6 },
			modelUsage: { [HAIKU]: { inputTokens: 16, outputTokens: 6 } } } }]);
		const response = await provider.step(request([user("go")]), 1, () => {});
		expect(response.reported_model).toBe(HAIKU);
		expect(response.usage).toEqual(totals);
		expect(steps[0].reported_model).toBe(HAIKU);
	});

	test("with no reported model the response carries none, so the host falls back to the alias", async () => {
		const { provider, steps } = setup([{ sessionId: "cc", turns: [{ model: undefined, text: "ok", usage }] }]);
		const response = await provider.step(request([user("go")]), 1, () => {});
		expect(response.reported_model).toBeUndefined();
		expect(response.text).toBe("ok");
		expect(steps[0].reported_model).toBeUndefined();
	});
});

describe("one-off summaries", () => {
	test("a one-off names the model its messages reported, with the aggregate totals", async () => {
		const { provider, steps } = setup([{ sessionId: "oneoff", turns: [{ model: HAIKU, text: "sum", usage }] }]);
		const response = await provider.step(sessionless(), 1, () => {});
		expect(response.reported_model).toBe(HAIKU);
		expect(response.usage).toEqual(totals);
		expect(steps[0]).toMatchObject({ path: "one-off", reported_model: HAIKU });
	});

	test("a one-off with only result totals names the SDK's modelUsage key", async () => {
		const { provider } = setup([{ sessionId: "oneoff", turns: [], result: { result: "sum", usage: {
			input_tokens: 10, cache_read_input_tokens: 4, cache_creation_input_tokens: 2, output_tokens: 6 },
			modelUsage: { [HAIKU]: { inputTokens: 16, outputTokens: 6 } } } }]);
		const response = await provider.step(sessionless(), 1, () => {});
		expect(response.reported_model).toBe(HAIKU);
		expect(response.usage).toEqual(totals);
	});
});

describe("failed calls with reported usage", () => {
	test("a persistent step that fails after streamed messages keeps their usage and model", async () => {
		const { provider, steps } = setup([{ sessionId: "cc", turns: [{ model: HAIKU, text: "partial", usage }],
			error: { result: "boom", subtype: "error_during_execution" } }]);
		const error = await provider.step(request([user("go")]), 1, () => {}).catch((e) => e);
		expect(error).toBeInstanceOf(ProviderError);
		expect(error.usage).toEqual(totals);
		expect(error.reportedModel).toBe(HAIKU);
		expect(error.data).toEqual({ kind: "fatal", usage: totals, reported_model: HAIKU });
		expect(steps[0]).toMatchObject({ error: { kind: "fatal" }, reported_model: HAIKU, usage: totals });
	});

	test("an error result carrying only its own totals reports them with the SDK's model", async () => {
		const { provider } = setup([{ sessionId: "cc", turns: [], error: { result: "boom", subtype: "error_during_execution",
			usage: { input_tokens: 9, output_tokens: 3, cache_read_input_tokens: 0 },
			modelUsage: { [HAIKU]: { inputTokens: 9, outputTokens: 3 } } } }]);
		const error = await provider.step(request([user("go")]), 1, () => {}).catch((e) => e);
		expect(error.usage).toEqual({ input_tokens: 9, output_tokens: 3, cache_read_tokens: 0, cache_creation_tokens: 0 });
		expect(error.reportedModel).toBe(HAIKU);
	});

	test("a thrown transport failure keeps the usage streamed before it", async () => {
		const { provider } = setup([{ sessionId: "cc", turns: [{ model: HAIKU, text: "partial", usage }], throwAfterTurns: "socket hang up" }]);
		const error = await provider.step(request([user("go")]), 1, () => {}).catch((e) => e);
		expect(error.kind).toBe("transient");
		expect(error.usage).toEqual(totals);
		expect(error.reportedModel).toBe(HAIKU);
	});

	test("a failed one-off keeps its aggregate usage and the reported model", async () => {
		const { provider } = setup([{ sessionId: "oneoff", turns: [], error: { result: "boom", subtype: "error_during_execution",
			usage: { input_tokens: 9, output_tokens: 3, cache_read_input_tokens: 0 },
			modelUsage: { [HAIKU]: { inputTokens: 9, outputTokens: 3 } } } }]);
		const error = await provider.step(sessionless(), 1, () => {}).catch((e) => e);
		expect(error.usage).toEqual({ input_tokens: 9, output_tokens: 3, cache_read_tokens: 0, cache_creation_tokens: 0 });
		expect(error.reportedModel).toBe(HAIKU);
	});
});

// A message can name its model without any usage (message_start or assistant frames
// with no counts). The name must survive; counts must never be invented for it.
describe("model named without usage", () => {
	const zero = { input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0 };
	const resultUsage = { input_tokens: 10, cache_read_input_tokens: 4, cache_creation_input_tokens: 2, output_tokens: 6 };

	test("a persistent step keeps the message's model and the result's aggregate usage", async () => {
		const { provider, steps } = setup([{ sessionId: "cc", turns: [{ model: HAIKU, text: "ok", noUsage: true }],
			result: { result: "ok", usage: resultUsage } }]);
		const response = await provider.step(request([user("go")]), 1, () => {});
		expect(response.reported_model).toBe(HAIKU);
		expect(response.usage).toEqual(totals);
		expect(steps[0]).toMatchObject({ reported_model: HAIKU, usage: totals });
	});

	test("the result's reasoning counter still lands when the message reported no usage", async () => {
		const { provider } = setup([{ sessionId: "cc", turns: [{ model: HAIKU, text: "ok", noUsage: true }],
			result: { result: "ok", usage: { ...resultUsage, output_tokens_details: { thinking_tokens: 3 } } } }]);
		const response = await provider.step(request([user("go")]), 1, () => {});
		expect(response.reported_model).toBe(HAIKU);
		expect(response.usage).toEqual({ ...totals, reasoning_tokens: 3 });
	});

	test("with no usage anywhere the model is kept and no counts are invented", async () => {
		const { provider, steps } = setup([{ sessionId: "cc", turns: [{ model: HAIKU, text: "ok", noUsage: true }] }]);
		const response = await provider.step(request([user("go")]), 1, () => {});
		expect(response.reported_model).toBe(HAIKU);
		expect(response.usage).toEqual(zero);
		expect(steps[0]).toMatchObject({ reported_model: HAIKU });
	});

	test("a failed persistent step keeps the model and reports no usage it never received", async () => {
		const { provider, steps } = setup([{ sessionId: "cc", turns: [{ model: HAIKU, text: "partial", noUsage: true }],
			error: { result: "boom", subtype: "error_during_execution" } }]);
		const error = await provider.step(request([user("go")]), 1, () => {}).catch((e) => e);
		expect(error).toBeInstanceOf(ProviderError);
		expect(error.usage).toBeUndefined();
		expect(error.reportedModel).toBe(HAIKU);
		expect(steps[0]).toMatchObject({ error: { kind: "fatal" }, reported_model: HAIKU });
	});

	test("a one-off keeps the message's model with the result's aggregate usage", async () => {
		const { provider, steps } = setup([{ sessionId: "oneoff", turns: [{ model: HAIKU, text: "sum", noUsage: true }],
			result: { result: "sum", usage: resultUsage } }]);
		const response = await provider.step(sessionless(), 1, () => {});
		expect(response.reported_model).toBe(HAIKU);
		expect(response.usage).toEqual(totals);
		expect(steps[0]).toMatchObject({ path: "one-off", reported_model: HAIKU });
	});

	test("a one-off with no usage anywhere keeps the model and invents no counts", async () => {
		const { provider } = setup([{ sessionId: "oneoff", turns: [{ model: HAIKU, text: "sum", noUsage: true }] }]);
		const response = await provider.step(sessionless(), 1, () => {});
		expect(response.reported_model).toBe(HAIKU);
		expect(response.usage).toEqual(zero);
	});

	test("a failed one-off keeps the model and reports no usage it never received", async () => {
		const { provider } = setup([{ sessionId: "oneoff", turns: [{ model: HAIKU, noUsage: true }],
			error: { result: "boom", subtype: "error_during_execution" } }]);
		const error = await provider.step(sessionless(), 1, () => {}).catch((e) => e);
		expect(error.usage).toBeUndefined();
		expect(error.reportedModel).toBe(HAIKU);
	});

	test("a synthetic marker or configured alias on a message without usage is never reported", async () => {
		for (const model of ["<synthetic>", "haiku"]) {
			const { provider } = setup([{ sessionId: "cc", turns: [{ model, text: "ok", noUsage: true }],
				result: { result: "ok", usage: resultUsage } }]);
			const response = await provider.step(request([user("go")]), 1, () => {});
			expect(response.reported_model).toBeUndefined();
			expect(response.usage).toEqual(totals);
		}
	});
});
