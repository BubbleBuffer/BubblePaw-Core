// Models and capabilities reported by `initialize`, and the mapping from a request's
// model id and reasoning effort onto agy's model ids.
//
// agy lists models with an effort suffix (`gemini-3.8-flash-low`, `claude-opus-5-5-high`)
// in the `session/new` reply and takes them in `session/set_model`. BubblePaw's
// model id is the base (`gemini-3.8-flash`) and `reasoning_effort` picks the suffix;
// an id that is already a full agy id is used as it is. Any other id is passed
// through unchanged.

import type { Capabilities, Effort, ModelEntry } from "./types.js";

/** agy's context window (`usage_update.size`, measured 2026-11-21). */
export const CONTEXT_WINDOW = 1_048_576;

/** Tokens agy puts in front of every conversation (its own system prompt),
 *  measured about 8.6k on 2026-11-21; the host adds this to its fork-size estimate. */
export const AGY_PROMPT_TOKENS = 8_600;

export function capabilities(): Capabilities {
	return { context_window: CONTEXT_WINDOW, cache: "session", tool_surface: { kind: "code" }, stateful: true };
}

const BASES: Array<[string, string]> = [
	["gemini-4-neon", "Gemini 4 Neon"],
	["gemini-4-argon", "Gemini 4 Argon"],
	["gemini-4-hydrogen", "Gemini 4 Hydrogen"],
	["gemini-3.8-flash", "Gemini 3.8 Flash"],
	["claude-opus-5-5", "Claude Opus 5.5 (via Antigravity)"],
	["claude-sonnet-5-5", "Claude Sonnet 5.5 (via Antigravity)"],
];

export const MODELS: ModelEntry[] = BASES.map(([id, name]) => ({ id, name, capabilities: capabilities() }));

const LADDER: Effort[] = ["low", "medium", "high", "xhigh"];

/** The agy model id for `model` and `effort`, given the ids the session offers.
 *  Unspecified effort means low. An effort the model lacks steps down the ladder
 *  (`max` is `xhigh`). Returns the id to send and the base id to report. */
export function resolveModel(available: readonly string[], model: string, effort: Effort | null | undefined): { id: string; base: string } {
	if (available.includes(model)) return { id: model, base: stripEffort(model) };
	let start = LADDER.indexOf(effort === "max" ? "xhigh" : (effort ?? "low"));
	if (start < 0) start = 0;
	for (let i = start; i >= 0; i--) {
		const id = `${model}-${LADDER[i]}`;
		if (available.includes(id)) return { id, base: model };
	}
	for (let i = start + 1; i < LADDER.length; i++) {
		const id = `${model}-${LADDER[i]}`;
		if (available.includes(id)) return { id, base: model };
	}
	return { id: model, base: model };
}

export function stripEffort(id: string): string {
	return id.replace(/-(low|medium|high|xhigh)$/, "");
}
