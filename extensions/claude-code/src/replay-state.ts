import type { Message, ToolCall } from "./types.js";

const FORMAT = "message-references-v1";
const KIND = "anthropic-messages";

/** The wire blocks keep opaque fields; ordinary text and calls live only in Message. */
export function replayState(content: readonly any[], text: string, calls: readonly ToolCall[], model: string): any {
	let offset = 0;
	const bytes = Buffer.from(text, "utf8");
	const refs: any[] = [];
	const blocks = content.map((original) => {
		const block = structuredClone(original);
		const ref: any = {};
		if (block.type === "text" && typeof block.text === "string") {
			const part = Buffer.from(block.text, "utf8");
			const end = offset + part.length;
			if (!bytes.subarray(offset, end).equals(part)) throw new Error("replay text differs from message text");
			ref.text = [offset, end];
			offset = end;
			delete block.text;
		}
		if (block.type === "tool_use") {
			const index = calls.findIndex((call) => call.id === block.id);
			if (index >= 0) {
				ref.call = index;
				delete block.id;
				delete block.name;
				delete block.input;
			}
		}
		refs.push(ref);
		return block;
	});
	if (offset !== bytes.length) throw new Error("replay text differs from message text");
	return { adapter: KIND, format: FORMAT, content: blocks, refs,
		_bubblepaw_origin: { provider_kind: KIND, model, source: "native" } };
}

export function replayCompatible(state: any, model?: string): boolean {
	if (state?.adapter !== KIND || !Array.isArray(state.content)) return false;
	const origin = state._bubblepaw_origin;
	return !origin || (origin.provider_kind === KIND && (origin.model == null || model == null || origin.model === model));
}

/** Legacy originless full blocks and current references share the same replay boundary. */
export function replayContent(message: Message, model?: string): any[] | undefined {
	const state = message.provider_state as any;
	if (!replayCompatible(state, model)) return undefined;
	const blocks = structuredClone(state.content);
	if (state.format !== FORMAT) return blocks;
	if (!Array.isArray(state.refs) || state.refs.length !== blocks.length) throw new Error("invalid replay reference count");
	const bytes = Buffer.from(message.content, "utf8");
	blocks.forEach((block: any, index: number) => {
		const ref = state.refs[index];
		if (block.type === "text") {
			const range = ref?.text;
			if (!Array.isArray(range) || range.length !== 2 || !range.every(Number.isSafeInteger)
				|| range[0] < 0 || range[1] < range[0] || range[1] > bytes.length) throw new Error("invalid replay text boundary");
			const part = bytes.subarray(range[0], range[1]);
			block.text = new TextDecoder("utf-8", { fatal: true }).decode(part);
		}
		if (ref?.call != null) {
			const call = message.tool_calls?.[ref.call];
			if (!Number.isSafeInteger(ref.call) || !call) throw new Error("missing replay tool call");
			Object.assign(block, { id: call.id, name: call.name, input: structuredClone(call.arguments === undefined ? {} : call.arguments) });
		}
	});
	return blocks;
}
