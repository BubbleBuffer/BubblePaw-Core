import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { convertMessages, oneOffPrompt, promptText, turnStart } from "../src/convert.js";
import { writeSession } from "../src/mirror.js";
import { assistant, system, tool, user } from "./helpers.js";

describe("message translation for a rebuild", () => {
	test("tool calls and results become tool_use and one grouped tool_result message", () => {
		const history = [
			system("Context chapters: earlier work"),
			user("read the file"),
			assistant("Reading.", [
				{ id: "toolu_1", name: "js", arguments: { code: "1" } },
				{ id: "call:2/x", name: "js", arguments: { code: "2" } },
			]),
			tool("toolu_1", "one"),
			tool("call:2/x", "boom", true),
			assistant("Done."),
		];
		const { messages, sanitizedIds } = convertMessages(history);
		expect(messages.map((m) => m.role)).toEqual(["user", "user", "assistant", "user", "assistant"]);
		expect(messages[0].content).toContain("<system-note>");
		const calls = messages[2].content as any[];
		expect(calls[0]).toEqual({ type: "text", text: "Reading." });
		expect(calls[1]).toEqual({ type: "tool_use", id: "toolu_1", name: "mcp__bubblepaw__js", input: { code: "1" } });
		expect(calls[2].id).toBe("call_2_x");
		expect(sanitizedIds.get("call:2/x")).toBe("call_2_x");
		const results = messages[3].content as any[];
		expect(results).toEqual([
			{ type: "tool_result", tool_use_id: "toolu_1", content: "one", is_error: false },
			{ type: "tool_result", tool_use_id: "call_2_x", content: "boom", is_error: true },
		]);
	});

	test("an aborted empty assistant turn is dropped, an orphan result is counted", () => {
		const { messages, dropped } = convertMessages([tool("x", "orphan"), user("hi"), assistant(""), user("again")]);
		expect(dropped.abortedTurns).toBe(1);
		expect(dropped.orphanResults).toBe(1);
		expect(messages.map((m) => m.role)).toEqual(["user", "user", "user"]);
	});

	test("history/prompt split: trailing user and system messages are the prompt", () => {
		const history = [user("a"), assistant("b"), user("c"), system("note")];
		expect(turnStart(history)).toBe(2);
		expect(promptText(history)).toBe("c\n\n<system-note>\nnote\n</system-note>");
		expect(promptText([user("a"), assistant("b")])).toBeNull();
	});

	test("one-off prompts", () => {
		expect(oneOffPrompt([user("summarize")])).toBe("summarize");
		expect(oneOffPrompt([user("a"), assistant("b", [{ id: "1", name: "js", arguments: {} }])])).toContain("[tool call js {}]");
	});

	test("writeSession writes a Claude Code JSONL that cc-session-io can read back", () => {
		const dir = mkdtempSync(join(tmpdir(), "bp-cc-"));
		try {
			const priors = [user("read it"), assistant("", [{ id: "toolu_a", name: "js", arguments: { code: "x" } }, { id: "toolu_b", name: "js", arguments: { code: "y" } }]), tool("toolu_a", "A"), tool("toolu_b", "B"), assistant("both done")];
			const written = writeSession({ priors, cwd: dir, claudeDir: dir, model: "haiku" });
			expect(written.warnings).toEqual([]);
			expect(written.sessionId).toBeTruthy();
			const projects = join(dir, "projects");
			const [project] = readdirSync(projects);
			const lines = readFileSync(join(projects, project, `${written.sessionId}.jsonl`), "utf8").trim().split("\n").map((l) => JSON.parse(l));
			const kinds = lines.map((r) => r.type);
			expect(kinds.filter((k) => k === "user" || k === "assistant")).toEqual(["user", "assistant", "user", "assistant"]);
			const resultRecord = lines.find((r) => r.type === "user" && Array.isArray(r.message.content) && r.message.content[0]?.type === "tool_result");
			expect(resultRecord.message.content.map((b: any) => b.tool_use_id)).toEqual(["toolu_a", "toolu_b"]);
			const callRecord = lines.find((r) => r.type === "assistant");
			expect(callRecord.message.content.map((b: any) => b.name)).toEqual(["mcp__bubblepaw__js", "mcp__bubblepaw__js"]);

			// Rewriting in place keeps the id; forceRotate takes a new one.
			const again = writeSession({ priors, cwd: dir, claudeDir: dir, previousId: written.sessionId! });
			expect(again.sessionId).toBe(written.sessionId);
			const rotated = writeSession({ priors, cwd: dir, claudeDir: dir, previousId: written.sessionId!, forceRotate: true });
			expect(rotated.sessionId).not.toBe(written.sessionId);
			expect(writeSession({ priors: [], cwd: dir, claudeDir: dir }).sessionId).toBeNull();
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});
});
