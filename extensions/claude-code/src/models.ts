// Models and capabilities reported by `initialize`.
//
// Adapted from pi-claude-bridge src/models.ts and its AskClaude model choice
// (`opus`, `sonnet`, `haiku`). pi-claude-bridge is MIT, Copyright (c) 2026 Eli
// Dickinson; see NOTICE.
//
// Changes from pcb: pcb takes its picker from pi-ai's Anthropic catalog, which
// BubblePaw does not have. We list the family aliases Claude Code resolves to its
// current model, plus the full ids pcb measured. pcb's measurement
// (diag/CONTEXT-SIZE.md): a bare id serves 200K; only the `[1m]` suffix requests
// 1M, and an unentitled `[1m]` id fails every turn, so 200K is the safe default.
// Any other id is passed through to Claude Code unchanged with the default
// capabilities.

import type { Capabilities, ModelEntry } from "./types.js";

export const TWO_HUNDRED_K = 200_000;
export const ONE_M = 1_000_000;

export function capabilities(contextWindow: number = TWO_HUNDRED_K): Capabilities {
	return { context_window: contextWindow, cache: "session", tool_surface: { kind: "code" }, stateful: true };
}

/** pcb's MEASURED_ONE_M: `[1m]` ids verified to serve 1M on every plan. */
const MEASURED_ONE_M = [
	["claude-fable-5-1", "Claude Fable 5.1 (1M)"],
	["claude-opus-5-5", "Claude Opus 5.5 (1M)"],
	["claude-sonnet-5-5", "Claude Sonnet 5.5 (1M)"],
] as const;

export const MODELS: ModelEntry[] = [
	{ id: "opus", name: "Claude Opus (latest)", capabilities: capabilities() },
	{ id: "sonnet", name: "Claude Sonnet (latest)", capabilities: capabilities() },
	{ id: "haiku", name: "Claude Haiku (latest)", capabilities: capabilities() },
	...MEASURED_ONE_M.map(([id, name]) => ({ id: `${id}[1m]`, name, capabilities: capabilities(ONE_M) })),
];

/** A full model ID as the Agent SDK reports it (`claude-haiku-5-5`). Configured
 *  aliases (`haiku`, `opus[1m]`) and markers (`<synthetic>`) never match. */
const REPORTED_MODEL = /^claude-[a-z0-9][a-z0-9.-]{0,120}$/;

export function reportedModelId(value: unknown): string | undefined {
	return typeof value === "string" && REPORTED_MODEL.test(value) ? value : undefined;
}

/** The reported model with the most output tokens, from (model, output) pairs.
 *  Pairs without a valid reported ID are ignored; ties keep the first seen. */
export function dominantModel(pairs: Iterable<[unknown, number]>): string | undefined {
	const output = new Map<string, number>();
	for (const [model, tokens] of pairs) {
		const id = reportedModelId(model);
		if (id !== undefined) output.set(id, (output.get(id) ?? 0) + tokens);
	}
	let best: string | undefined;
	let most = -1;
	for (const [id, tokens] of output) {
		if (tokens > most) {
			best = id;
			most = tokens;
		}
	}
	return best;
}
