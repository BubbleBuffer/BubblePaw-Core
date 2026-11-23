// The loopback channel between the extension and the MCP shim agy spawns.
// agy starts stdio MCP servers itself, so the `bubblepaw` MCP server is a tiny
// process (mcp-shim.ts) that connects back here: a named pipe on Windows, a unix
// socket elsewhere, one NDJSON line per message, a per-session random token to
// say which session it serves. The shim forwards `tools/list` and `tools/call`;
// this side answers from the session registered under the token.

import { randomBytes } from "node:crypto";
import { existsSync, unlinkSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { McpResult, ToolDefinition } from "./types.js";

const MAX_LINE_BYTES = 16 * 1024 * 1024;

export interface BridgeSession {
	tools(): ToolDefinition[];
	/** A tools/call from agy: resolves when BubblePaw delivered the result. */
	call(name: string, args: unknown): Promise<McpResult>;
}

export function loopbackPath(): string {
	const name = `bubblepaw-agy-${randomBytes(6).toString("hex")}`;
	return process.platform === "win32" ? `\\\\.\\pipe\\${name}` : join(tmpdir(), `${name}.sock`);
}

export class Bridge {
	private readonly sessions = new Map<string, BridgeSession>();
	private server?: net.Server;
	readonly path = loopbackPath();

	async start(): Promise<void> {
		if (this.server) return;
		this.server = net.createServer((socket) => this.serve(socket));
		await new Promise<void>((resolve, reject) => {
			this.server!.once("error", reject);
			this.server!.listen(this.path, () => resolve());
		});
	}

	register(session: BridgeSession): string {
		const token = randomBytes(24).toString("hex");
		this.sessions.set(token, session);
		return token;
	}

	unregister(token: string): void {
		this.sessions.delete(token);
	}

	close(): void {
		this.server?.close();
		this.server = undefined;
		if (process.platform !== "win32" && existsSync(this.path)) {
			try {
				unlinkSync(this.path);
			} catch {}
		}
	}

	private serve(socket: net.Socket): void {
		let buffer = "";
		let bufferBytes = 0;
		socket.setEncoding("utf8");
		socket.on("error", () => {});
		socket.on("data", (chunk: string) => {
			if (socket.destroyed) return;
			let start = 0;
			while (start < chunk.length) {
				const end = chunk.indexOf("\n", start);
				const part = chunk.slice(start, end < 0 ? chunk.length : end);
				bufferBytes += Buffer.byteLength(part, "utf8");
				if (bufferBytes > MAX_LINE_BYTES) {
					buffer = "";
					socket.destroy();
					return;
				}
				buffer += part;
				if (end < 0) return;
				const line = buffer;
				buffer = "";
				bufferBytes = 0;
				if (line.trim()) this.onLine(socket, line);
				start = end + 1;
			}
		});
	}

	private onLine(socket: net.Socket, line: string): void {
		let m: any;
		try {
			m = JSON.parse(line);
		} catch {
			return;
		}
		if (!m || typeof m !== "object" || Array.isArray(m)) return;
		const reply = (body: unknown) => {
			if (!socket.destroyed) socket.write(`${JSON.stringify({ id: m.id, ...(body as object) })}\n`);
		};
		const session = typeof m.token === "string" ? this.sessions.get(m.token) : undefined;
		if (!session) return reply({ error: "unknown session token" });
		if (m.op === "list") return reply({ tools: session.tools() });
		if (m.op === "call") {
			session.call(String(m.name), m.arguments).then(
				(result) => reply({ result }),
				(error) => reply({ result: { content: [{ type: "text", text: String(error?.message ?? error) }], isError: true } }),
			);
			return;
		}
		reply({ error: "unknown op" });
	}
}
