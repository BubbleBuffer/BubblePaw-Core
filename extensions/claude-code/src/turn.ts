// Per-step turn state of the step engine (provider.ts): what one Claude Code
// step has streamed so far, and the usage it reported.

import { ProviderError, type ToolCall, type Usage } from "./types.js";
import { dominantModel, reportedModelId } from "./models.js";

export type Block =
	| { type: "text"; text: string; native?: any; index?: number }
	| { type: "tool"; id: string; name: string; partialJson: string; args: unknown; native?: any; index?: number }
	| { type: "opaque"; native: any; index?: number };

export interface MessageUsage {
	input: number;
	output: number;
	cacheRead: number;
	cacheWrite: number;
	reasoning?: number;
}

/** Per-step turn state (pcb's per-turn QueryContext fields). */
export class Turn {
	blocks: Block[] = [];
	sawStream = false;
	streamMessageId: string | undefined;
	streamOpen = false;
	blockStart = 0;
	usage = new Map<string, MessageUsage>();
	/** The full model ID each API message reported. Kept apart from `usage`: a
	 *  message can name its model with no counts, and that name must survive. */
	messageModels = new Map<string, string>();
	/** The SDK's per-model result totals for its whole query (`result.modelUsage`). */
	resultModels: Record<string, { outputTokens?: number }> | undefined;
	syntheticText = "";
	lastMessageId: string | undefined;
	/** Ids of the user messages this step pushed into the running query. */
	steered: string[] = [];

	toolCalls(): ToolCall[] {
		return this.blocks.filter((b): b is Extract<Block, { type: "tool" }> => b.type === "tool").map((b) => ({ id: b.id, name: b.name, arguments: b.args }));
	}
	text(): string {
		return this.blocks.filter((b): b is Extract<Block, { type: "text" }> => b.type === "text").map((b) => b.text).join("");
	}
	content(): any[] {
		return this.blocks.map((block) => block.type === "text"
			? { ...(block.native ?? {}), type: "text", text: block.text }
			: block.type === "tool"
				? { ...(block.native ?? {}), type: "tool_use", id: block.id, name: block.name, input: block.args }
				: structuredClone(block.native));
	}
	recordUsage(messageId: string | undefined, usage: any, model?: string): void {
		const key = messageId ?? this.streamMessageId ?? "(unknown)";
		const reported = reportedModelId(model);
		if (reported !== undefined) this.messageModels.set(key, reported);
		if (!usage) return;
		const current = this.usage.get(key) ?? { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
		// Frames carry different subsets; an omitted counter keeps its value (pcb usage.ts).
		if (usage.input_tokens != null) current.input = usage.input_tokens;
		if (usage.output_tokens != null) current.output = usage.output_tokens;
		if (usage.cache_read_input_tokens != null) current.cacheRead = usage.cache_read_input_tokens;
		if (usage.cache_creation_input_tokens != null) current.cacheWrite = usage.cache_creation_input_tokens;
		if (usage.output_tokens_details?.thinking_tokens != null) current.reasoning = usage.output_tokens_details.thinking_tokens;
		this.usage.set(key, current);
	}
	/** Summed over the API messages of this step (more than one only when Claude
	 *  Code retried inside the step). `input_tokens` is the whole prompt, cache
	 *  reads and writes included, as the Responses provider reports it. */
	totals(): Usage {
		let input = 0, output = 0, cacheRead = 0, cacheWrite = 0;
		let reasoning: number | undefined;
		for (const u of this.usage.values()) {
			input += u.input + u.cacheRead + u.cacheWrite;
			output += u.output;
			cacheRead += u.cacheRead;
			cacheWrite += u.cacheWrite;
			if (u.reasoning !== undefined) reasoning = (reasoning ?? 0) + u.reasoning;
		}
		return { input_tokens: input, output_tokens: output, cache_read_tokens: cacheRead, cache_creation_tokens: cacheWrite,
			...(reasoning !== undefined ? { reasoning_tokens: reasoning } : {}) };
	}
	/** Native structured output is the canonical reply; retain every opaque block. */
	structuredOutput(value: unknown): void {
		const text = JSON.stringify(value);
		if (text === undefined) throw new ProviderError("fatal", "Claude Code returned an invalid structured_output");
		const first = this.blocks.findIndex((block) => block.type === "text");
		if (first < 0) this.blocks.push({ type: "text", text });
		else {
			this.blocks[first] = { ...this.blocks[first], type: "text", text };
			for (const [index, block] of this.blocks.entries()) if (block.type === "text" && index !== first) block.text = "";
		}
	}
	/** SDK result usage covers the query's main loop, including already returned
	 *  tool steps. Use its genuinely reported final reasoning counter as a delta. */
	resultUsage(usage: any, returned: Usage, modelUsage?: Record<string, { outputTokens?: number }>): void {
		if (modelUsage) this.resultModels = modelUsage;
		if (!usage) return;
		if (this.usage.size === 0) {
			const priorRawInput = returned.input_tokens - (returned.cache_read_tokens ?? 0) - (returned.cache_creation_tokens ?? 0);
			const subtract = (value: number | undefined, prior: number) => value == null ? undefined : Math.max(0, value - prior);
			this.recordUsage("(result)", {
				input_tokens: subtract(usage.input_tokens, priorRawInput),
				output_tokens: subtract(usage.output_tokens, returned.output_tokens),
				cache_read_input_tokens: subtract(usage.cache_read_input_tokens, returned.cache_read_tokens ?? 0),
				cache_creation_input_tokens: subtract(usage.cache_creation_input_tokens, returned.cache_creation_tokens ?? 0),
			});
		}
		const thinking = usage.output_tokens_details?.thinking_tokens;
		if (thinking != null) {
			for (const recorded of this.usage.values()) delete recorded.reasoning;
			this.recordUsage("(result reasoning)", { output_tokens_details: { thinking_tokens: Math.max(0, thinking - (returned.reasoning_tokens ?? 0)) } });
		}
	}
	/** The full model ID that served this step: the API messages' own IDs (the
	 *  one with most output when several), else the SDK's per-model result totals. */
	reportedModel(): string | undefined {
		return dominantModel([...this.messageModels].map(([key, model]): [string, number] => [model, this.usage.get(key)?.output ?? 0]))
			?? dominantModel(Object.entries(this.resultModels ?? {}).map(([model, u]): [string, number] => [model, u?.outputTokens ?? 0]));
	}
	/** Drop blocks of a stream Claude Code abandoned mid-message (no message_stop):
	 *  a tool call there is one it will never dispatch (pcb). */
	dropAbandoned(): void {
		this.blocks.splice(this.blockStart);
		this.streamOpen = false;
	}
}

/** A failed call still names what it used: the usage it reported and the model
 *  the SDK named. Failed calls are metered, not dropped. */
export function reportedFailure(error: ProviderError, turn: Turn): ProviderError {
	if (error.usage === undefined && turn.usage.size > 0) error.usage = turn.totals();
	if (error.reportedModel === undefined) error.reportedModel = turn.reportedModel();
	return error;
}
