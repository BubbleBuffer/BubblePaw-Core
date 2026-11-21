// A minimal ACP (Agent Client Protocol) client over a child process's stdio:
// NDJSON JSON-RPC, the agent's requests (permissions) answered by a handler,
// its notifications (session/update) handed to a listener. It knows nothing about
// BubblePaw; the provider owns what the messages mean.

import { spawn, type ChildProcess } from "node:child_process";
import { logStderr } from "./errors.js";
import { LineSplitter } from "./rpc.js";

export interface AcpOptions {
	command: string;
	args: string[];
	env: Record<string, string>;
	cwd: string;
	/** An agent-to-client request (e.g. `session/request_permission`); the return value is the result. */
	onRequest: (method: string, params: any) => Promise<unknown> | unknown;
	/** An agent-to-client notification (e.g. `session/update`). */
	onNotify: (method: string, params: any) => void;
	onExit: (reason: string) => void;
	log: (line: string) => void;
}

export class AcpProcess {
	private nextId = 1;
	private readonly pending = new Map<number, { resolve: (value: any) => void; reject: (error: unknown) => void }>();
	private readonly child: ChildProcess;
	private readonly splitter = new LineSplitter();
	alive = true;

	constructor(private readonly opts: AcpOptions) {
		this.child = spawn(opts.command, opts.args, { cwd: opts.cwd, env: opts.env, stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
		this.child.stdout!.setEncoding("utf8");
		this.child.stdout!.on("data", (chunk: string) => {
			for (const line of this.splitter.push(chunk)) this.onLine(line);
		});
		this.child.stderr!.setEncoding("utf8");
		this.child.stderr!.on("data", (data: string) => logStderr(data, opts.log));
		this.child.stdin!.on("error", () => {});
		this.child.on("error", (error) => this.died(`spawn failed: ${error.message}`));
		this.child.on("exit", (code, signal) => this.died(`process exited (${code ?? signal})`));
	}

	private died(reason: string): void {
		if (!this.alive) return;
		this.alive = false;
		for (const waiting of this.pending.values()) waiting.reject(new Error(`agy ${reason}`));
		this.pending.clear();
		this.opts.onExit(reason);
	}

	private write(record: unknown): void {
		if (!this.alive) return;
		this.child.stdin!.write(`${JSON.stringify(record)}\n`);
	}

	private onLine(line: string): void {
		let m: any;
		try {
			m = JSON.parse(line);
		} catch {
			return;
		}
		if (m.method && m.id !== undefined && m.id !== null) {
			Promise.resolve()
				.then(() => this.opts.onRequest(m.method, m.params))
				.then(
					(result) => this.write({ jsonrpc: "2.0", id: m.id, result: result ?? {} }),
					(error) => this.write({ jsonrpc: "2.0", id: m.id, error: { code: -32603, message: String(error?.message ?? error) } }),
				);
		} else if (m.method) {
			try {
				this.opts.onNotify(m.method, m.params);
			} catch (error) {
				this.opts.log(`notification ${m.method} failed: ${(error as Error).message}`);
			}
		} else if (m.id !== undefined) {
			const waiting = this.pending.get(m.id);
			if (!waiting) return;
			this.pending.delete(m.id);
			if (m.error) waiting.reject(Object.assign(new Error(m.error.message ?? "agy error"), { err: m.error }));
			else waiting.resolve(m.result);
		}
	}

	request<T = any>(method: string, params: unknown): Promise<T> {
		if (!this.alive) return Promise.reject(new Error("agy process exited"));
		const id = this.nextId++;
		return new Promise<T>((resolve, reject) => {
			this.pending.set(id, { resolve, reject });
			this.write({ jsonrpc: "2.0", id, method, params });
		});
	}

	notify(method: string, params: unknown): void {
		this.write({ jsonrpc: "2.0", method, params });
	}

	kill(): void {
		this.alive = false;
		try {
			this.child.kill();
		} catch {}
	}
}
