// JSON-RPC 2.0 over stdio, one record per line (docs/providers.md, "Out-of-process
// extensions"). Pure framing: the dispatcher hands requests to a handler and writes
// whatever it returns; nothing here knows about Claude Code.
//
// Part of bubblepaw-claude-code, whose provider mechanism is adapted from
// pi-claude-bridge (MIT, Copyright (c) 2026 Eli Dickinson); see NOTICE.

import { ProviderError } from "./types.js";
import { classifyThrown } from "./errors.js";

export type RpcId = number | string;

export interface RpcMessage {
	jsonrpc?: string;
	id?: RpcId | null;
	method?: string;
	params?: any;
}

/** Splits a byte/character stream into complete lines; keeps the partial tail. */
export class LineSplitter {
	private buffer = "";
	push(chunk: string): string[] {
		this.buffer += chunk;
		const lines = this.buffer.split("\n");
		this.buffer = lines.pop() ?? "";
		return lines.map((line) => line.replace(/\r$/, "")).filter((line) => line.trim().length > 0);
	}
	/** Whatever is left once the input closed without a final newline. */
	flush(): string[] {
		const rest = this.buffer.trim();
		this.buffer = "";
		return rest ? [rest] : [];
	}
}

export type Write = (line: string) => void;

export function encode(value: unknown): string {
	// JSON.stringify never emits a raw newline (they are escaped inside strings), so
	// one record is always exactly one line.
	return `${JSON.stringify(value)}\n`;
}

export function resultRecord(id: RpcId, result: unknown) {
	return { jsonrpc: "2.0", id, result };
}

export function errorRecord(id: RpcId | null, error: unknown) {
	if (error instanceof ProviderError) {
		return { jsonrpc: "2.0", id, error: { code: -32000, message: error.message, data: error.data } };
	}
	const failure = classifyThrown(error);
	return { jsonrpc: "2.0", id, error: { code: -32000, message: failure.message, data: failure.data } };
}

export function notification(method: string, params: unknown) {
	return { jsonrpc: "2.0", method, params };
}

export interface Handlers {
	/** Requests: the returned value (or thrown error) becomes the reply. */
	request(method: string, params: any, id: RpcId): Promise<unknown>;
	/** Notifications: no reply. */
	notify(method: string, params: any): void;
}

/** Dispatch one parsed line. Requests run concurrently; replies go through `write`. */
export function dispatchLine(line: string, handlers: Handlers, write: Write): void {
	let message: RpcMessage;
	try {
		message = JSON.parse(line);
	} catch {
		write(encode({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error", data: { kind: "fatal" } } }));
		return;
	}
	if (!message || typeof message.method !== "string") return; // a stray reply; we send no requests
	if (message.id === undefined || message.id === null) {
		try {
			handlers.notify(message.method, message.params ?? {});
		} catch (error) {
			process.stderr.write(`notification failed: ${classifyThrown(error).message}\n`);
		}
		return;
	}
	const id = message.id;
	handlers
		.request(message.method, message.params ?? {}, id)
		.then((result) => write(encode(resultRecord(id, result ?? {}))))
		.catch((error) => write(encode(errorRecord(id, error))));
}
