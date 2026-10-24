// In-process MCP server that serves a step's tools to Claude Code, and the
// parking of tool calls until BubblePaw delivers their results.
//
// Adapted from pi-claude-bridge src/mcp-server.ts (createToolServer) and
// src/index.ts (buildMcpServers, deliverToolResults, releasePendingToolCalls).
// pi-claude-bridge is MIT, Copyright (c) 2026 Eli Dickinson; see NOTICE.
//
// Schemas are served verbatim as JSON Schema rather than through the SDK's
// Zod-only `createSdkMcpServer`, whose round trip is lossy below the top level
// (pcb's finding). Handlers go on the protocol server directly, which also skips
// argument validation: BubblePaw validates and runs tools itself.
//
// Changes from pcb: tool definitions come from the BubblePaw request
// (`input_schema`), and the parking lot is its own class so it can be tested
// without the SDK.

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import type { ToolDefinition } from "./types.js";

/** Claude Code stamps every tools/call with the tool_use id it came from. That is
 *  the only reliable pairing: call order need not match tool_use order. Not part
 *  of the MCP spec, so if Claude Code stops sending it every call fails loudly. */
export const TOOL_USE_ID_META = "claudecode/toolUseId";

export interface McpResult {
	content: Array<{ type: "text"; text: string }>;
	isError?: boolean;
}

/** Parked MCP tool calls and early results, paired by tool_use id. A handler that
 *  runs first parks its resolver; a result that lands first waits for its handler. */
export class ParkingLot {
	readonly pending = new Map<string, { toolName: string; resolve: (result: McpResult) => void }>();
	readonly early = new Map<string, McpResult>();
	/** Called whenever a tool call parks (for diagnostics and tests). */
	onPark?: (toolUseId: string, toolName: string) => void;

	park(toolUseId: string, toolName: string): Promise<McpResult> {
		const ready = this.early.get(toolUseId);
		if (ready) {
			this.early.delete(toolUseId);
			return Promise.resolve(ready);
		}
		return new Promise<McpResult>((resolve) => {
			this.pending.set(toolUseId, { toolName, resolve });
			this.onPark?.(toolUseId, toolName);
		});
	}

	/** Release one call with its result, or queue the result for a later handler. */
	deliver(toolUseId: string, result: McpResult): "released" | "queued" {
		const waiting = this.pending.get(toolUseId);
		if (waiting) {
			this.pending.delete(toolUseId);
			waiting.resolve(result);
			return "released";
		}
		this.early.set(toolUseId, result);
		return "queued";
	}

	/** Answer every parked handler with `reason`. Resolved, never rejected: an error
	 *  reply is still a reply, and a handler left waiting on a process that is gone
	 *  keeps Claude Code's tools/call open forever (pcb). */
	releaseAll(reason: string): void {
		for (const waiting of this.pending.values()) waiting.resolve({ content: [{ type: "text", text: reason }], isError: true });
		this.pending.clear();
		this.early.clear();
	}
}

/** MCP requires object schemas; reject others at query start, naming the tool. */
function assertObjectSchema(tool: ToolDefinition): void {
	const schema = tool.input_schema as Record<string, unknown> | undefined;
	if (!schema || schema.type !== "object") {
		throw new Error(`${tool.name}: MCP tool parameters must be an object schema, got ${JSON.stringify(schema)}`);
	}
}

export function createToolServer(name: string, tools: readonly ToolDefinition[], lot: ParkingLot) {
	const server = new McpServer({ name, version: "1.0.0" }, { capabilities: { tools: {} } });
	const byName = new Map(tools.map((tool) => [tool.name, tool]));
	for (const tool of tools) assertObjectSchema(tool);

	server.server.setRequestHandler(ListToolsRequestSchema, () => ({
		tools: tools.map((tool) => ({
			name: tool.name,
			description: tool.description,
			inputSchema: tool.input_schema as Record<string, unknown> as any,
		})),
	}));

	server.server.setRequestHandler(CallToolRequestSchema, async (request) => {
		const tool = byName.get(request.params.name);
		if (!tool) throw new Error(`Unknown tool: ${request.params.name}`);
		const toolUseId = (request.params._meta as Record<string, unknown> | undefined)?.[TOOL_USE_ID_META];
		if (typeof toolUseId !== "string") {
			throw new Error(`${tool.name}: tools/call is missing _meta["${TOOL_USE_ID_META}"]; cannot pair the result with its tool call`);
		}
		const { content, isError } = await lot.park(toolUseId, tool.name);
		return { content, isError };
	});

	return { type: "sdk" as const, name, instance: server };
}
