// The stdio MCP server agy spawns for the `bubblepaw` server. It holds no logic:
// it speaks just enough MCP (initialize, tools/list, tools/call, ping) and
// forwards tool listing and calls to the extension over the loopback channel
// (bridge.ts). Environment: BUBBLEPAW_AGY_PIPE (channel path), BUBBLEPAW_AGY_TOKEN.

import net from "node:net";
import { LineSplitter } from "./rpc.js";

const path = process.env.BUBBLEPAW_AGY_PIPE ?? "";
const token = process.env.BUBBLEPAW_AGY_TOKEN ?? "";

const channel = net.connect(path);
channel.setEncoding("utf8");
const waiting = new Map<number, (reply: any) => void>();
let nextId = 1;
const channelLines = new LineSplitter();
channel.on("data", (chunk: string) => {
	for (const line of channelLines.push(chunk)) {
		try {
			const reply = JSON.parse(line);
			waiting.get(reply.id)?.(reply);
			waiting.delete(reply.id);
		} catch {}
	}
});
channel.on("error", () => process.exit(1));
channel.on("close", () => process.exit(0));

function ask(body: object): Promise<any> {
	const id = nextId++;
	return new Promise((resolve) => {
		waiting.set(id, resolve);
		channel.write(`${JSON.stringify({ id, token, ...body })}\n`);
	});
}

const send = (record: unknown): void => {
	process.stdout.write(`${JSON.stringify(record)}\n`);
};

async function onMessage(m: any): Promise<void> {
	const id = m.id;
	if (id === undefined || id === null) return; // notifications need no reply
	switch (m.method) {
		case "initialize":
			return send({ jsonrpc: "2.0", id, result: { protocolVersion: m.params?.protocolVersion ?? "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "bubblepaw", version: "1.0.0" } } });
		case "ping":
			return send({ jsonrpc: "2.0", id, result: {} });
		case "tools/list": {
			const reply = await ask({ op: "list" });
			const tools = (reply.tools ?? []).map((t: any) => ({ name: t.name, description: t.description, inputSchema: t.input_schema }));
			return send({ jsonrpc: "2.0", id, result: { tools } });
		}
		case "tools/call": {
			const reply = await ask({ op: "call", name: m.params?.name, arguments: m.params?.arguments ?? {} });
			if (reply.error) return send({ jsonrpc: "2.0", id, error: { code: -32603, message: String(reply.error) } });
			return send({ jsonrpc: "2.0", id, result: reply.result });
		}
		default:
			return send({ jsonrpc: "2.0", id, error: { code: -32601, message: `method not found: ${m.method}` } });
	}
}

const lines = new LineSplitter();
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk: string) => {
	for (const line of lines.push(chunk)) {
		try {
			void onMessage(JSON.parse(line));
		} catch {}
	}
});
process.stdin.on("end", () => process.exit(0));
