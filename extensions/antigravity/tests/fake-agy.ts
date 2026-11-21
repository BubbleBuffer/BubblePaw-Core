// A scripted fake `agy acp`: ACP over stdio, no model. The "model" is the prompt
// text itself:
//   CALL <tool> <json>   call that bubblepaw MCP tool (through the real MCP shim,
//                        which agy would spawn), then answer `got: <result text>`
//   BUILTIN              ask permission for a built-in tool and an MCP tool, answer
//                        with the outcomes
//   HANG                 never finish (until session/cancel)
//   DIE                  exit the process
//   anything else        answer `echo`
// The directive is looked for after "Current user input:" when the prompt is a seed.
// Every message the fake receives goes to the file named by FAKE_AGY_LOG.

import { spawn, type ChildProcess } from "node:child_process";
import { appendFileSync } from "node:fs";

const LOG = process.env.FAKE_AGY_LOG ?? "";
const log = (record: unknown): void => {
	if (LOG) appendFileSync(LOG, `${JSON.stringify(record)}\n`);
};
const send = (record: unknown): void => {
	process.stdout.write(`${JSON.stringify(record)}\n`);
};

let nextOutId = 100;
const waiting = new Map<number, (value: any) => void>();
const askClient = (method: string, params: unknown) =>
	new Promise<any>((resolve) => {
		const id = nextOutId++;
		waiting.set(id, resolve);
		send({ jsonrpc: "2.0", id, method, params });
	});

const MODELS = ["gemini-3.8-flash-low", "gemini-3.8-flash-medium", "gemini-3.8-flash-high", "claude-opus-5-5-low"].map((modelId) => ({ modelId, name: modelId }));

interface Session {
	mcp?: { command: string; args: string[]; env: Array<{ name: string; value: string }> };
	shim?: ChildProcess;
	shimWaiting: Map<number, (m: any) => void>;
	shimBuf: string;
	shimId: number;
	release?: () => void;
}
const sessions = new Map<string, Session>();
let count = 0;

function shimRequest(s: Session, method: string, params: unknown): Promise<any> {
	const id = s.shimId++;
	return new Promise((resolve) => {
		s.shimWaiting.set(id, resolve);
		s.shim!.stdin!.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
	});
}

async function startShim(s: Session): Promise<void> {
	if (s.shim || !s.mcp) return;
	const env = { ...process.env, ...Object.fromEntries(s.mcp.env.map((e) => [e.name, e.value])) };
	s.shim = spawn(s.mcp.command, s.mcp.args, { env, stdio: ["pipe", "pipe", "ignore"] });
	s.shim.stdout!.setEncoding("utf8");
	s.shim.stdout!.on("data", (chunk: string) => {
		s.shimBuf += chunk;
		let i: number;
		while ((i = s.shimBuf.indexOf("\n")) >= 0) {
			const line = s.shimBuf.slice(0, i);
			s.shimBuf = s.shimBuf.slice(i + 1);
			const m = JSON.parse(line);
			s.shimWaiting.get(m.id)?.(m);
			s.shimWaiting.delete(m.id);
		}
	});
	await shimRequest(s, "initialize", { protocolVersion: "2024-11-05" });
	const listed = await shimRequest(s, "tools/list", {});
	log({ shimTools: listed.result.tools.map((t: any) => t.name) });
}

const chunk = (sessionId: string, text: string) => send({ jsonrpc: "2.0", method: "session/update", params: { sessionId, update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text } } } });
const usage = { inputTokens: 100, outputTokens: 7, cachedReadTokens: 40, cachedWriteTokens: 0, thoughtTokens: 3, totalTokens: 107 };

async function prompt(id: number, params: any): Promise<void> {
	const sessionId: string = params.sessionId;
	const s = sessions.get(sessionId)!;
	const text: string = params.prompt.map((p: any) => p.text ?? "").join("");
	const directive = text.includes("Current user input:") ? text.slice(text.lastIndexOf("Current user input:")) : text;
	const finish = (stopReason = "end_turn") => send({ jsonrpc: "2.0", id, result: { stopReason, usage } });
	if (/\bDIE\b/.test(directive)) process.exit(1);
	if (/\bHANG\b/.test(directive)) {
		await new Promise<void>((resolve) => (s.release = resolve));
		return finish("cancelled");
	}
	const call = directive.match(/CALL (\w+) (\{.*\})/);
	if (call) {
		chunk(sessionId, "calling. ");
		const outcome = await askClient("session/request_permission", { sessionId, toolCall: { name: `mcp_bubblepaw_${call[1]}` }, options: [{ optionId: "allow", kind: "allow_once" }, { optionId: "deny", kind: "reject_once" }] });
		if (outcome.outcome?.optionId !== "allow") {
			chunk(sessionId, "permission refused");
			return finish();
		}
		await startShim(s);
		const reply = await shimRequest(s, "tools/call", { name: call[1], arguments: JSON.parse(call[2]) });
		log({ toolResult: reply.result ?? reply.error });
		chunk(sessionId, `got: ${reply.result?.content?.[0]?.text ?? "error"}`);
		return finish();
	}
	if (/\bBUILTIN\b/.test(directive)) {
		const options = [{ optionId: "allow", kind: "allow_once" }, { optionId: "deny", kind: "reject_once" }];
		const builtin = await askClient("session/request_permission", { sessionId, toolCall: { name: "run_command" }, options });
		const mcp = await askClient("session/request_permission", { sessionId, toolCall: { name: "mcp_bubblepaw_lookup" }, options });
		chunk(sessionId, `builtin=${builtin.outcome.optionId} mcp=${mcp.outcome.optionId}`);
		return finish();
	}
	chunk(sessionId, "echo");
	return finish();
}

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (data: string) => {
	buffer += data;
	let i: number;
	while ((i = buffer.indexOf("\n")) >= 0) {
		const line = buffer.slice(0, i);
		buffer = buffer.slice(i + 1);
		if (!line.trim()) continue;
		const m = JSON.parse(line);
		log(m.method ? { method: m.method, params: m.params } : { reply: m.result });
		if (m.method === undefined) {
			waiting.get(m.id)?.(m.result);
			waiting.delete(m.id);
		} else if (m.method === "initialize") send({ jsonrpc: "2.0", id: m.id, result: { protocolVersion: 1, agentCapabilities: {} } });
		else if (m.method === "session/new") {
			const sessionId = `fake-${++count}`;
			sessions.set(sessionId, { mcp: m.params.mcpServers?.[0], shimWaiting: new Map(), shimBuf: "", shimId: 1 });
			send({ jsonrpc: "2.0", id: m.id, result: { sessionId, models: { availableModels: MODELS, currentModelId: MODELS[0].modelId } } });
		} else if (m.method === "session/prompt") void prompt(m.id, m.params);
		else if (m.method === "session/cancel") sessions.get(m.params.sessionId)?.release?.();
		else if (m.method === "session/set_model" || m.method === "session/close") send({ jsonrpc: "2.0", id: m.id, result: {} });
	}
});
process.stdin.on("end", () => {
	for (const s of sessions.values()) s.shim?.kill();
	process.exit(0);
});
