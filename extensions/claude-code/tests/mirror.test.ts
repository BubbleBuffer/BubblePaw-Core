import { describe, expect, test } from "bun:test";
import { CONTINUE_PROMPT, decide, isEphemeral, stableCount, type LiveQuery, type Mirror } from "../src/mirror.js";
import { assistant, note, request, system, tool, user } from "./helpers.js";

const state = (tool_use_ids: string[], session_id = "cc1") => ({ claude_code: { session_id, tool_use_ids } });

function mirror(cursor: number, toolUseIds: string[], extra: Partial<Mirror> = {}): Mirror {
	return { ccSessionId: "cc1", epoch: "e1", cursor, lastStep: { toolUseIds }, needsRebuild: false, forceRotate: false, cwd: ".", ...extra };
}
const live = { alive: true, model: "haiku", toolNames: ["js"] };

describe("reuse versus rebuild from the cursor", () => {
	const u = user("go");
	const calls = [{ id: "t1", name: "js", arguments: {} }, { id: "t2", name: "js", arguments: {} }];
	const a = assistant("", calls, state(["t1", "t2"]));
	const r1 = tool("t1", "one");
	const r2 = tool("t2", "two");

	test("no mirror: rebuild (fresh when there is no history before the prompt)", () => {
		const d = decide(undefined, request([u]), undefined);
		expect(d).toMatchObject({ kind: "rebuild", priors: [], prompt: "go", reason: "no mirror" });
	});

	test("same epoch, only the tool results appended: deliver to the parked query", () => {
		const d = decide(mirror(1, ["t1", "t2"]), request([u, a, r2, r1]), live);
		expect(d.kind).toBe("deliver");
		if (d.kind === "deliver") expect(d.results.map((m) => m.tool_call_id)).toEqual(["t2", "t1"]);
	});

	test("same epoch, final reply then a new prompt: reuse with the prompt", () => {
		const final = assistant("done", [], state([]));
		const d = decide(mirror(4, []), request([u, a, r1, r2, final, user("next")]), undefined);
		expect(d).toEqual({ kind: "reuse", prompt: "next" });
	});

	test("a new epoch rebuilds from the request", () => {
		const d = decide(mirror(1, ["t1", "t2"]), request([u, a, r1, r2], "e2"), live);
		expect(d).toMatchObject({ kind: "rebuild", reason: "new epoch", prompt: CONTINUE_PROMPT });
		if (d.kind === "rebuild") expect(d.priors.length).toBe(4);
	});

	test("inconsistencies rebuild", () => {
		const cases: Array<[Mirror, ReturnType<typeof request>, LiveQuery | undefined, string]> = [
			[mirror(1, ["t1", "t2"], { needsRebuild: true }), request([u, a, r1, r2]), live, "needs rebuild"],
			[mirror(1, ["t1", "t2"]), request([u, assistant("", calls, state(["t1", "t2"], "other")), r1, r2]), live, "last step's reply does not match the mirror"],
			[mirror(1, ["t1", "t2"]), request([u, a, r1]), live, "tool results do not match the parked calls"],
			[mirror(1, ["t1", "t2"]), request([u, a, r1, r2, system("x")]), live, "messages besides tool results and user messages"],
			[mirror(1, ["t1", "t2"]), request([u, a, r1, user("steer"), r2]), live, "messages besides tool results and user messages"],
			[mirror(1, ["t1", "t2"]), request([u, a, r1, r2, user("steer")]), { ...live, steerable: false }, "steering needs a prompt stream"],
			[mirror(1, ["t1", "t2"]), request([u, a, r1, r2]), { ...live, alive: false }, "parked query is gone"],
			[mirror(1, ["t1", "t2"]), request([u, a, r1, r2], "e1", { model: "sonnet" }), live, "model changed"],
			[mirror(1, ["t1", "t2"]), request([u, a, r1, r2], "e1", { tools: [] }), live, "tools changed"],
			[mirror(5, []), request([u, a, r1, r2]), undefined, "last step's reply missing"],
			[mirror(4, []), request([u, a, r1, r2, assistant("done", [], state([]))]), undefined, "no new prompt after the last reply"],
		];
		for (const [m, req, l, reason] of cases) expect(decide(m, req, l)).toMatchObject({ kind: "rebuild", reason });
	});
});

describe("ephemeral messages and steering", () => {
	const u = user("go");
	const calls = [{ id: "t1", name: "js", arguments: {} }];
	const a = assistant("", calls, state(["t1"]));
	const r1 = tool("t1", "one");
	const steerable = { ...live, steerable: true };

	test("ephemeral messages are recognised by id and not counted", () => {
		expect(isEphemeral(note("n"))).toBe(true);
		expect(isEphemeral(system("s"))).toBe(false);
		expect(stableCount([u, note("n"), a])).toBe(2);
	});

	test("deliver ignores ephemeral messages for the cursor and hands them over as notes", () => {
		const d = decide(mirror(1, ["t1"]), request([note("old position"), u, a, r1, note("jobs: 1")]), steerable);
		expect(d.kind).toBe("deliver");
		if (d.kind === "deliver") {
			expect(d.results).toEqual([r1]);
			expect(d.steer).toEqual([]);
			expect(d.notes.map((m) => m.content)).toEqual(["old position", "jobs: 1"]);
		}
	});

	test("reuse puts the notes before the next user text", () => {
		const final = assistant("done", [], state([]));
		const d = decide(mirror(3, []), request([u, a, r1, final, user("next"), note("jobs: 2")]), undefined);
		expect(d).toEqual({ kind: "reuse", prompt: "<system-note>\njobs: 2\n</system-note>\n\nnext" });
	});

	test("a rebuild keeps the current notes at their position; a notes-only turn gets the continue prompt", () => {
		const d = decide(undefined, request([u, a, r1, note("jobs: 3")]), undefined);
		expect(d).toMatchObject({ kind: "rebuild", prompt: `<system-note>\njobs: 3\n</system-note>\n\n${CONTINUE_PROMPT}` });
		if (d.kind === "rebuild") expect(d.priors).toEqual([u, a, r1]);
		const withPrompt = decide(undefined, request([u, note("jobs: 4")]), undefined);
		expect(withPrompt).toMatchObject({ kind: "rebuild", priors: [], prompt: "go\n\n<system-note>\njobs: 4\n</system-note>" });
	});

	test("user messages after the tool results are a steer", () => {
		const s1 = user("also check B");
		const s2 = user("and C");
		const d = decide(mirror(1, ["t1"]), request([u, a, r1, s1, s2, note("n")]), steerable);
		expect(d).toMatchObject({ kind: "deliver", results: [r1], steer: [s1, s2] });
	});
});
