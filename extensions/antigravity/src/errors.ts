// agy (ACP) failures -> typed provider errors (docs/providers.md): not logged in
// is `auth`, quota is `rate_limit`, a too-long prompt is `context`, process and
// network failures are `transient`, the rest `fatal` with one redacted,
// truncated line of the underlying text (see `diagnosticLine`).

import { ProviderError, type ErrorKind } from "./types.js";

export const LOGIN_HINT = "Antigravity is not logged in: run `agy` once in a terminal and log in, then retry";

const AUTH = /not logged in|log ?in|sign ?in|unauthenticated|authentication|auth(orization)? required|credentials|permission_denied|\b(401|403)\b|unauthorized|forbidden/i;
const LIMIT = /quota|rate limit|resource_exhausted|\b429\b|too many requests|usage limit|exhausted/i;
const CONTEXT = /context (length|window)|too many tokens|token limit|prompt is too long|exceeds the maximum/i;
const TRANSIENT = /overloaded|unavailable|\b5\d\d\b|timed? ?out|ECONN|ENOTFOUND|EAI_AGAIN|socket hang up|network|fetch failed|exited|terminated|spawn|EPIPE|connection|deadline/i;

const SAFE: Record<ErrorKind, string> = {
	auth: LOGIN_HINT,
	rate_limit: "Antigravity rate limit or quota reached",
	context: "Antigravity context limit exceeded",
	transient: "Antigravity transport failed",
	fatal: "Antigravity request failed",
};

const MAX_DIAGNOSTIC_CHARS = 300;
const SECRET_ENV_NAME = /key|token|secret|password|passwd|auth|credential|cookie|session/i;
const REDACTIONS: [RegExp, string][] = [
	[/\b(bearer|basic)\s+[A-Za-z0-9_.\/+=:-]+/gi, "$1 [redacted]"],
	[/\b(authorization|proxy-authorization|x-api-key|api[-_]?key|(?:access|refresh|id)?[-_]?token|secret|client[-_]?secret|password|cookie|set-cookie)\b(\s*["']?\s*[:=]\s*["']?)[^"'\s,;}\]]+/gi, "$1$2[redacted]"],
	[/\b([A-Z][A-Z0-9_]{2,})=("[^"]*"|'[^']*'|\S+)/g, "$1=[redacted]"],
	[/\bAIza[A-Za-z0-9_-]{20,}/g, "[redacted]"],
	[/\bya29\.[A-Za-z0-9_.-]+/g, "[redacted]"],
	[/\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_.-]+/g, "[redacted]"],
	[/[A-Za-z0-9_+=-]{40,}/g, "[redacted]"],
];

function envSecrets(env: Record<string, string | undefined> = {}): string[] {
	return Object.entries({ ...process.env, ...env })
		.filter(([name, value]) => SECRET_ENV_NAME.test(name) && typeof value === "string" && value.length >= 8)
		.map(([, value]) => value as string);
}

/** The first nonempty line of `text`, whitespace-collapsed, with keys, tokens,
 *  `NAME=value` assignments, long opaque strings and credential-named env values
 *  replaced by `[redacted]`, then cut to 300 characters. Redaction runs first. */
export function diagnosticLine(text: string, env?: Record<string, string | undefined>): string {
	let line = text.split(/\r?\n/).map((part) => part.trim()).find((part) => part.length > 0) ?? "";
	for (const secret of envSecrets(env)) line = line.split(secret).join("[redacted]");
	for (const [pattern, replacement] of REDACTIONS) line = line.replace(pattern, replacement);
	line = line.replace(/\s+/g, " ");
	return line.length > MAX_DIAGNOSTIC_CHARS ? `${line.slice(0, MAX_DIAGNOSTIC_CHARS - 12)} [truncated]` : line;
}

export function classify(text: string): ProviderError {
	if (AUTH.test(text)) return new ProviderError("auth", SAFE.auth);
	if (CONTEXT.test(text)) return new ProviderError("context", SAFE.context);
	if (LIMIT.test(text)) return new ProviderError("rate_limit", SAFE.rate_limit);
	if (TRANSIENT.test(text)) return new ProviderError("transient", SAFE.transient);
	const detail = diagnosticLine(text);
	return new ProviderError("fatal", detail ? `${SAFE.fatal}: ${detail}` : SAFE.fatal);
}

export function classifyThrown(error: unknown): ProviderError {
	if (error instanceof ProviderError) return error;
	const rpc = (error as { err?: { message?: string; data?: unknown } } | null)?.err;
	const text = rpc ? `${rpc.message ?? ""} ${typeof rpc.data === "string" ? rpc.data : JSON.stringify(rpc.data ?? "")}` : error instanceof Error ? error.message : String(error);
	return classify(text);
}

export function cancelled(): ProviderError {
	return new ProviderError("transient", "cancelled");
}

/** agy's own stderr, one redacted, truncated log line per line. */
export function logStderr(data: string, log: (line: string) => void): void {
	for (const raw of String(data).split(/\r?\n/)) {
		const line = diagnosticLine(raw);
		if (line) log(`agy stderr: ${line}`);
	}
}
