// Protocol types for the BubblePaw provider-extension protocol (docs/providers.md).
// These are the serde forms of bubblepaw-core's ModelRequest/ModelResponse/Message.
//
// Part of bubblepaw-claude-code, whose provider mechanism is adapted from
// pi-claude-bridge (MIT, Copyright (c) 2026 Eli Dickinson); see NOTICE.

import type { RateLimitInfo } from "./quota.js";
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

export interface ModelRequest {
	model: string;
	system_prompt: string;
	messages: Message[];
	tools: ToolDefinition[];
	max_output_tokens?: number | null;
	reasoning_effort?: "low" | "medium" | "high" | "xhigh" | "max" | null;
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
	quota_events?: RateLimitInfo[];
	/** The full model ID the SDK reported for this request (`claude-haiku-5-5`);
	 *  the host names the usage row with it. Absent when the SDK reported none. */
	reported_model?: string;
}

export type ErrorKind = "auth" | "rate_limit" | "context" | "transient" | "fatal";

/** A typed provider failure; becomes a JSON-RPC error with `data: {kind, resets_at?}`. */
export class ProviderError extends Error {
	quotaEvents: RateLimitInfo[] = [];
	/** Usage the failed request reported before it failed; the host meters it. */
	usage?: Usage;
	/** The full model ID reported before the failure, when the SDK named one. */
	reportedModel?: string;
	constructor(
		readonly kind: ErrorKind,
		message: string,
		readonly resetsAt?: string,
		/** Subscription usage cap that rejected the request (Claude Code's
		 *  `rateLimitType`, e.g. `five_hour`); absent for a plain rate limit. */
		readonly limitType?: string,
	) {
		super(message);
		this.name = "ProviderError";
	}
	get data(): { kind: ErrorKind; resets_at?: string; limit_type?: string; quota_events?: RateLimitInfo[]; terminal_quota?: boolean; usage?: Usage; reported_model?: string } {
		return {
			kind: this.kind,
			...(this.resetsAt ? { resets_at: this.resetsAt } : {}),
			...(this.limitType ? { limit_type: this.limitType } : {}),
			...(this.quotaEvents.length ? { quota_events: this.quotaEvents } : {}),
			...(this.message.startsWith("billing or spend limit reached") ? { terminal_quota: true } : {}),
			...(this.usage ? { usage: this.usage } : {}),
			...(this.reportedModel ? { reported_model: this.reportedModel } : {}),
		};
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
