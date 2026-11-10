import { describe, expect, test } from "bun:test";
import { getSessionPath } from "cc-session-io";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { forkCut } from "../src/fork.js";
import type { Mirror } from "../src/mirror.js";
import { ClaudeCodeProvider } from "../src/provider.js";
import { ProviderError } from "../src/types.js";
import { fakeQuery } from "./fake-sdk.js";
import { assistant, request, system, tool, user } from "./helpers.js";

// A session file as Claude Code writes it: one record per content block of an
// assistant message, tool results in a user record (a runtime note rides along),
// and records that are no conversation (queue operations, attachments).
const rec = (type: "user" | "assistant", uuid: string, content: any[] | string, mid?: string, extra: object = {}) =>
	({ type, uuid, parentUuid: null, sessionId: "cc1", ...(mid ? { message: { id: mid, role: type, content } } : { message: { role: type, content } }), ...extra });
const FILE = [
	{ type: "queue-operation", operation: "enqueue" },
	rec("user", "u1", "go"),
	{ type: "attachment", uuid: "att1", attachment: { type: "x" } },
	rec("assistant", "a1", [{ type: "tool_use", id: "t1", name: "mcp__bubblepaw__js", input: {} }], "m1"),
	rec("user", "r1", [{ type: "tool_result", tool_use_id: "t1", content: "one" }, { type: "text", text: "<system-note>\nnote\n</system-note>" }]),
	rec("assistant", "a2k", [{ type: "thinking", thinking: "hm", signature: "s" }], "m2"),
	rec("assistant", "a2t", [{ type: "text", text: "done" }], "m2"),
	rec("user", "u2", "next: ZEBRAQUILT"),
	rec("assistant", "a3", [{ type: "text", text: "later" }], "m3"),
];
const calls = [{ id: "t1", name: "js", arguments: {} }];
const history = [user("go"), assistant("", calls), tool("t1", "one"), assistant("done"), user("next: ZEBRAQUILT"), assistant("later")];

describe("the fork cut", () => {
	test("is the last record of the message it ends, whatever was written after it", () => {
		expect(forkCut(FILE, history.slice(0, 4))).toBe("a2t"); // thinking and text records are one message
		expect(forkCut(FILE, history.slice(0, 3))).toBe("r1"); // a result, with the turn's runtime note
		expect(forkCut(FILE, history.slice(0, 2))).toBe("a1");
	});

	test("is refused unless the file holds exactly that conversation up to it", () => {
		const cases: Array<[string, any[], any[]]> = [
			["at a user message", FILE, history.slice(0, 1)],
			["a tool id the file lacks", FILE, [user("go"), assistant("", [{ id: "other", name: "js", arguments: {} }])]],
			["a reply not fully written", FILE.filter((r) => r.uuid !== "a2t"), history.slice(0, 4)],
			["a message the file lacks", FILE.slice(0, 5), history.slice(0, 4)],
			["a steering text in the cut's record", [...FILE.slice(0, 4), rec("user", "r1", [{ type: "tool_result", tool_use_id: "t1", content: "one" }, { type: "text", text: "stop that" }]), ...FILE.slice(5)], history.slice(0, 3)],
		];
		for (const [name, records, prefix] of cases) {
			expect(() => forkCut(records, prefix), name).toThrow(ProviderError);
		}
		// Only the cuts no later flush can fix are reported as impossible (the host then summarizes that episode without a fork).
		const kinds = cases.map(([, records, prefix]) => { try { forkCut(records, prefix); } catch (e) { return (e as ProviderError).forkImpossible; } });
		expect(kinds).toEqual([true, false, false, false, true]);
	});

	test("takes a system message and a user message as one turn", () => {
		expect(forkCut(FILE, [system("summary"), user("go"), ...history.slice(1, 4)])).toBe("a2t");
	});
});

describe("a summary fork on the step engine", () => {
	function fixture(script: Parameters<typeof fakeQuery>[0]) {
		const dir = mkdtempSync(join(tmpdir(), "fork-"));
		const mirror: Mirror = { ccSessionId: "cc1", epoch: "e1", cursor: 5, lastStep: { toolUseIds: [] }, model: "haiku", needsRebuild: false, forceRotate: false, cwd: dir };
		const path = getSessionPath("cc1", dir, dir);
		mkdirSync(dirname(path), { recursive: true });
		writeFileSync(path, FILE.map((r) => JSON.stringify(r)).join("\n") + "\n");
		const fake = fakeQuery(script);
		const provider = new ClaudeCodeProvider({ query: fake.fn, cwd: dir, claudeDir: dir });
		provider.mirrors.set("s", mirror);
		return { provider, fake, mirror, before: { ...mirror } };
	}
	const control = "EPISODE MEMORY BRANCH ...\n\nHOST-GENERATED SUMMARY RETURN CONTROL ...";
	const fork = (messages: any[]) => request([...messages, user(control)], "e1", { history: undefined });

	test("resumes a fork of the parent cut after the episode, as one user message, and leaves the mirror alone", async () => {
		const { provider, fake, mirror, before } = fixture([{ sessionId: "fork1", turns: [{ text: "SUMMARY", usage: { input_tokens: 3, cache_read_input_tokens: 900, cache_creation_input_tokens: 40, output_tokens: 7 } }] }]);
		const response = await provider.summaryFork(fork(history.slice(0, 4)), 1, () => {});
		expect(response.text).toBe("SUMMARY");
		expect(response.usage).toMatchObject({ cache_read_tokens: 900, cache_creation_tokens: 40 });
		const [run] = fake.runs;
		expect(run.prompt).toBe(control);
		expect(run.options).toMatchObject({ resume: "cc1", forkSession: true, resumeSessionAt: "a2t", persistSession: false, permissionMode: "default", model: "haiku" });
		expect(run.options.allowDangerouslySkipPermissions).toBeUndefined();
		expect(Object.keys(run.options.mcpServers)).toEqual(["bubblepaw"]);
		expect(provider.mirrors.get("s")).toBe(mirror);
		expect(mirror).toEqual(before);
		expect(provider.lives.size).toBe(0);
	});

	test("a tool call in the fork's reply fails the call and runs nothing", async () => {
		const { provider, fake } = fixture([{ sessionId: "fork1", turns: [{ tools: [{ id: "toolu_x", name: "js", input: { code: "rm -rf /" } }] }] }]);
		const error = await provider.summaryFork(fork(history.slice(0, 4)), 1, () => {}).catch((e) => e);
		expect(error).toBeInstanceOf(ProviderError);
		expect(error.kind).toBe("fatal");
		const [run] = fake.runs;
		// Nothing answered the call but an error, and the SDK's permission hook refuses any call.
		expect(run.toolResults.every((r) => r.result.isError)).toBe(true);
		expect(await run.options.canUseTool("mcp__bubblepaw__js", {})).toMatchObject({ behavior: "deny" });
		expect(run.closed).toBe(true);
	});

	test("without a consistent cut it fails before starting any query, impossible only at a user message", async () => {
		const { provider, fake } = fixture([]);
		const error = await provider.summaryFork(fork([user("go")]), 1, () => {}).catch((e) => e);
		expect([error.kind, error.forkImpossible]).toEqual(["fatal", true]);
		expect(fake.runs).toHaveLength(0);
		const none = await new ClaudeCodeProvider({ query: fake.fn }).summaryFork(fork(history.slice(0, 4)), 2, () => {}).catch((e) => e);
		expect([none.kind, none.forkImpossible]).toEqual(["transient", false]);
	});
});
