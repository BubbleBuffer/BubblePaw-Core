// Protocol types for the BubblePaw provider-extension protocol (docs/providers.md):
// the serde forms of bubblepaw-core's ModelRequest/ModelResponse/Message.

export type Role = "system" | "user" | "assistant" | "tool";

export interface ToolCall {
	id: string;
	name: string;
	arguments: unknown;
}

export interface Message {
	id: string;
	session_id?: string;
	role: Role;
	content: string;
	tool_calls?: ToolCall[];
	tool_call_id?: string | null;
	provider_state?: unknown;
	is_error?: boolean;
	created_at?: string;
}

export interface ToolDefinition {
	name: string;
	description: string;
	input_schema: unknown;
	read_only?: boolean;
}

export interface HistoryCursor {
	epoch: string;
	rewritten_from?: string | null;
}

export type Effort = "low" | "medium" | "high" | "xhigh" | "max";

export interface ModelRequest {
	model: string;
	system_prompt: string;
	messages: Message[];
	tools: ToolDefinition[];
	max_output_tokens?: number | null;
	reasoning_effort?: Effort | null;
	output_schema?: { name: string; schema: unknown } | null;
	session_id?: string;
	history?: HistoryCursor;
}

export interface Usage {
	input_tokens: number;
	output_tokens: number;
	cache_read_tokens?: number;
	cache_creation_tokens?: number;
	reasoning_tokens?: number;
}

export interface ModelResponse {
	text: string;
	tool_calls: ToolCall[];
	usage: Usage | null;
	provider_state?: unknown;
	reported_model?: string;
}

export type ErrorKind = "auth" | "rate_limit" | "context" | "transient" | "fatal";

/** A typed provider failure; becomes a JSON-RPC error with `data: {kind}`. */
export class ProviderError extends Error {
	constructor(
		readonly kind: ErrorKind,
		message: string,
	) {
		super(message);
		this.name = "ProviderError";
	}
	get data(): { kind: ErrorKind } {
		return { kind: this.kind };
	}
}

export interface Capabilities {
	context_window: number | null;
	cache: "none" | "prefix" | "session";
	tool_surface: { kind: "direct" } | { kind: "code" };
	stateful: boolean;
}

export interface ModelEntry {
	id: string;
	name: string;
	capabilities: Capabilities;
}

/** What the MCP shim hands back for one tool call. */
export interface McpResult {
	content: Array<{ type: "text"; text: string }>;
	isError?: boolean;
}
