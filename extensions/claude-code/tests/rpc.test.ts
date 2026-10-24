import { describe, expect, test } from "bun:test";
import { handlers } from "../src/main.js";
import { ClaudeCodeProvider } from "../src/provider.js";
import { dispatchLine, encode, LineSplitter, type Handlers } from "../src/rpc.js";
import { ProviderError } from "../src/types.js";
import { fakeQuery } from "./fake-sdk.js";
import { request, user } from "./helpers.js";

async function settle() {
	await Bun.sleep(5);
}

describe("JSON-RPC framing", () => {
	test("lines split across chunks, CRLF and blank lines", () => {
		const s = new LineSplitter();
		expect(s.push('{"a":1}\r\n{"b"')).toEqual(['{"a":1}']);
		expect(s.push(':2}\n\n')).toEqual(['{"b":2}']);
		expect(s.push('{"c":3}')).toEqual([]);
		expect(s.flush()).toEqual(['{"c":3}']);
	});

	test("one record per line, even with newlines in strings", () => {
		const line = encode({ text: "a\nb" });
		expect(line.endsWith("\n")).toBe(true);
		expect(line.slice(0, -1).includes("\n")).toBe(false);
	});

	test("requests get results or typed errors; notifications get nothing", async () => {
		const out: string[] = [];
		const seen: string[] = [];
		const h: Handlers = {
			async request(method) {
				if (method === "ok") return { fine: true };
				throw new ProviderError("rate_limit", "limited", "2026-10-08T14:00:00Z");
			},
			notify(method, params) {
				seen.push(`${method}:${params.id}`);
			},
		};
		dispatchLine('{"jsonrpc":"2.0","id":1,"method":"ok"}', h, (l) => out.push(l));
		dispatchLine('{"jsonrpc":"2.0","id":2,"method":"bad"}', h, (l) => out.push(l));
		dispatchLine('{"jsonrpc":"2.0","method":"cancel","params":{"id":7}}', h, (l) => out.push(l));
		dispatchLine("not json", h, (l) => out.push(l));
		await settle();
		const records = out.map((l) => JSON.parse(l));
		expect(records).toContainEqual({ jsonrpc: "2.0", id: 1, result: { fine: true } });
		expect(records).toContainEqual({ jsonrpc: "2.0", id: 2, error: { code: -32000, message: "limited", data: { kind: "rate_limit", resets_at: "2026-10-08T14:00:00Z" } } });
		expect(records.find((r) => r.id === null).error.code).toBe(-32700);
		expect(seen).toEqual(["cancel:7"]);
	});

	test("initialize, step with text notifications, session.close", async () => {
		const fake = fakeQuery([{ sessionId: "cc1", turns: [{ text: "hello world" }] }]);
		const provider = new ClaudeCodeProvider({ query: fake.fn, cwd: "." });
		const out: string[] = [];
		const h = handlers(provider, (l) => out.push(l));
		dispatchLine('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"host":"bubblepaw","version":"0"}}', h, (l) => out.push(l));
		dispatchLine(JSON.stringify({ jsonrpc: "2.0", id: 2, method: "step", params: request([user("hi")]) }), h, (l) => out.push(l));
		await Bun.sleep(100);
		dispatchLine('{"jsonrpc":"2.0","id":3,"method":"session.close","params":{"session_id":"s"}}', h, (l) => out.push(l));
		await settle();
		const records = out.map((l) => JSON.parse(l));
		const init = records.find((r) => r.id === 1).result;
		expect(init.provider).toBe("claude-code");
		expect(init.capabilities).toEqual({ output_schema: true, reasoning_effort: true });
		expect(init.models.map((m: any) => m.id)).toContain("haiku");
		expect(init.models[0].capabilities).toEqual({ context_window: 200000, cache: "session", tool_surface: { kind: "code" }, stateful: true });
		const texts = records.filter((r) => r.method === "text");
		expect(texts.every((r) => r.params.id === 2)).toBe(true);
		expect(texts.map((r) => r.params.text).join("")).toBe("hello world");
		expect(records.find((r) => r.id === 2).result.text).toBe("hello world");
		expect(records.find((r) => r.id === 3).result).toEqual({});
	});
});
