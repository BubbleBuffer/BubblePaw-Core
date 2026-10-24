// Claude Code failures -> typed provider errors (docs/providers.md):
// not signed in is `auth`, a usage limit is `rate_limit` (with `resets_at` when
// Claude Code reported one, and `limit_type` when a subscription usage cap
// rejected the request), prompt too long is `context`, process and network
// failures are `transient`, the rest `fatal`.
//
// Adapted from pi-claude-bridge src/index.ts (resultErrorText, the
// rate_limit_event handling in consumeQuery, describeRateLimitFailure).
// pi-claude-bridge is MIT, Copyright (c) 2026 Eli Dickinson; see NOTICE.

import { ProviderError, type ErrorKind } from "./types.js";
import { terminalRateLimit, type RateLimitInfo } from "./quota.js";

/** What a query saw before failing. Claude Code sends a `rate_limit_event` with
 *  status `rejected` just before the failure it causes, which is the only thing
 *  tying the two together (pcb). */
export interface FailureContext {
	/** `error` field of the last assistant message (SDKAssistantMessageError). */
	assistantError?: string;
	rateLimitRejection?: Partial<RateLimitInfo> | null;
}

/** Failure text for an SDK `result`, or undefined when it succeeded (pcb). */
export function resultErrorText(message: any): string | undefined {
	if (message?.subtype === "error_max_budget_usd") return "error_max_budget_usd";
	if (message?.subtype === "success") return message.is_error ? message.result || "Claude Code reported an error" : undefined;
	if (Array.isArray(message?.errors) && message.errors.length) return message.errors.map(String).join("\n");
	if (typeof message?.error === "string") return message.error;
	return `Claude Code failed: ${message?.subtype ?? "unknown result"}`;
}

/** Unix seconds (pcb: unit undocumented in the SDK, observed) -> RFC 3339. */
export function resetsAtIso(resetsAt: number | undefined): string | undefined {
	if (resetsAt === undefined || !Number.isFinite(resetsAt) || resetsAt < 0 || resetsAt > 253402300799) return undefined;
	return new Date(resetsAt * 1000).toISOString().replace(/\.\d{3}Z$/, "Z");
}

const AUTH = /not logged in|please run \/login|invalid api key|authentication[_ ]failed|oauth token (has )?(expired|revoked)|\b(401|403)\b|unauthorized|forbidden|oauth_org_not_allowed|account_on_hold|verification_required/i;
const LIMIT = /usage limit|rate limit|out of (extra )?usage|limit reached|resets \d|429\b|too many requests/i;
const CONTEXT = /prompt is too long|context (length|window)|too many tokens|input length and `max_tokens` exceed/i;
const TRANSIENT = /overloaded|\b5\d\d\b|server error|timed? ?out|ECONN|ENOTFOUND|EAI_AGAIN|socket hang up|network|fetch failed|exited with code|process exited|terminated|spawn|EPIPE|connection/i;
const BILLING = /enforced_spend_limit_reached|billing_error|insufficient[_ ](quota|credits)|credits_required|credit balance is too low|out of credits|spend(ing)? limit reached|billing limit|error_max_budget_usd/i;

const SAFE_MESSAGES: Record<ErrorKind, string> = {
	auth: "Claude Code authentication failed",
	rate_limit: "Claude Code rate limit exceeded",
	context: "Claude Code context limit exceeded",
	transient: "Claude Code transport failed",
	fatal: "Claude Code request failed",
};

function safeFailure(kind: ErrorKind, resetsAt?: string, limitType?: string): ProviderError {
	return new ProviderError(kind, SAFE_MESSAGES[kind], resetsAt, limitType);
}

/** The subscription cap named by a `rate_limit_event` rejection; `subscription`
 *  when Claude Code did not name one. Only short identifiers pass. */
export function limitTypeOf(rejection: { rateLimitType?: string } | null | undefined): string | undefined {
	if (!rejection) return undefined;
	const type = rejection.rateLimitType;
	return typeof type === "string" && /^[a-z0-9_]{1,40}$/.test(type) ? type : "subscription";
}

export function classify(text: string, ctx: FailureContext = {}): ProviderError {
	const assistant = ctx.assistantError;
	if (assistant === "billing_error" || BILLING.test(text) || (ctx.rateLimitRejection && terminalRateLimit(ctx.rateLimitRejection as RateLimitInfo))) {
		return new ProviderError("fatal", "billing or spend limit reached");
	}
	if (assistant === "authentication_failed" || assistant === "oauth_org_not_allowed" || assistant === "account_on_hold" || assistant === "verification_required") {
		return safeFailure("auth");
	}
	const rejection = ctx.rateLimitRejection;
	if (rejection || assistant === "rate_limit") {
		const resetsAt = resetsAtIso(rejection?.resetsAt);
		return safeFailure("rate_limit", resetsAt, limitTypeOf(rejection));
	}
	if (AUTH.test(text)) return safeFailure("auth");
	if (CONTEXT.test(text)) return safeFailure("context");
	if (LIMIT.test(text)) return safeFailure("rate_limit");
	if (assistant === "overloaded" || assistant === "server_error") return safeFailure("transient");
	if (TRANSIENT.test(text)) return safeFailure("transient");
	return safeFailure("fatal");
}

/** A thrown error from the SDK or the Claude Code process. */
export function classifyThrown(error: unknown, ctx: FailureContext = {}): ProviderError {
	if (error instanceof ProviderError) return error;
	const text = error instanceof Error ? error.message : String(error);
	return classify(text, ctx);
}

export function cancelled(): ProviderError {
	return new ProviderError("transient", "cancelled");
}
