// The step engine: drives Claude Code one model step at a time.
//
// Adapted from pi-claude-bridge src/index.ts (streamClaudeAgentSdk, consumeQuery,
// processStreamEvent, processAssistantMessage, dropAbandonedStreamBlocks,
// discardRewrittenQuery, runIsolatedSummary) and src/query-state.ts.
// pi-claude-bridge is MIT, Copyright (c) 2026 Eli Dickinson; see NOTICE.
//
// Mechanism (as pcb): one Claude Code query per run of steps. A step that ends in
// tool calls returns them as soon as the assistant message stops; Claude Code's
// tools/call for each is parked in the MCP server. The next step carries the
// results as `tool` messages, which release the parked calls, and the same query
// goes on to the next assistant message. A step that ends without tool calls
// returns at the query's `result`, and the next prompt resumes the session.
//
// Changes from pcb: pi's event stream is replaced by one promise per step, text
// deltas go out as `text` notifications, the reuse/rebuild decision comes from
// the harness cursor (mirror.ts), steering happens only at parked tool calls
// (user messages arriving with the tool results), and there is no AskClaude,
// prompt capture, attachment carrying or skills handling.

import { query as sdkQuery } from "@anthropic-ai/claude-agent-sdk";
import { appendFileSync } from "node:fs";
import { bubblepawToolName, MCP_SERVER_NAME, oneOffPrompt } from "./convert.js";
import { cancelled, classify, classifyThrown, resultErrorText, type FailureContext } from "./errors.js";
import { createToolServer, ParkingLot } from "./mcp-server.js";
import { decide, notesText, stableCount, writeSession, type Decision, type Mirror } from "./mirror.js";
import { makePromptStream, userMessage, type PromptStream } from "./prompt-stream.js";
import { replayState } from "./replay-state.js";
import { assertUnchangedOptions, requestOptions, type RequestOptions } from "./request-options.js";
import { ProviderError, type ModelRequest, type ModelResponse, type ToolCall, type Usage } from "./types.js";
import { rateLimitInfo, type RateLimitInfo } from "./quota.js";
import { dominantModel, reportedModelId } from "./models.js";

export type QueryFn = (args: { prompt: any; options: any }) => AsyncIterable<any> & {
	interrupt?: () => Promise<unknown>;
	close?: () => void;
};

/** Applied to every Claude Code process (pcb's CC_CHILD_ENV): claude.ai MCP
 *  servers stay out, and BubblePaw owns compaction. */
export const CC_CHILD_ENV = { ENABLE_CLAUDEAI_MCP_SERVERS: "0", DISABLE_AUTO_COMPACT: "1" } as const;

/** BubblePaw owns context files; Claude Code must not load its own (pcb). */
export const CLAUDE_MD_EXCLUDES = ["**/CLAUDE.md", "**/AGENTS.md", "**/.claude/rules/**"];

/** Variables that would switch Claude Code from the subscription login to API
 *  billing. They are dropped from the child env unless this instance's
 *  configured `env` sets them explicitly (docs/providers.md). */
export const API_BILLING_ENV = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] as const;

/** Set by the BubblePaw host: the names this instance's `env` table sets. */
export const EXPLICIT_ENV_KEYS = "BUBBLEPAW_EXTENSION_ENV_KEYS";

/** How BubblePaw's system prompt reaches Claude Code: appended to the
 *  `claude_code` preset (default), or replacing it as a plain string. */
export type SystemPromptMode = "preset-append" | "replace";

export function parseSystemPromptMode(value: string | undefined): SystemPromptMode {
	if (value === undefined || value === "" || value === "preset-append") return "preset-append";
	if (value === "replace") return "replace";
	throw new Error(`BUBBLEPAW_CC_SYSTEM_PROMPT must be preset-append or replace, not ${JSON.stringify(value)}`);
}

/** The Claude Code child env: the extension's env without inherited API
 *  billing variables, plus CC_CHILD_ENV. */
export function childEnv(source: Record<string, string | undefined> = process.env): Record<string, string | undefined> {
	const explicit = new Set((source[EXPLICIT_ENV_KEYS] ?? "").split(",").filter(Boolean));
	const env = { ...source };
	for (const name of API_BILLING_ENV) if (!explicit.has(name)) delete env[name];
	return { ...env, ...CC_CHILD_ENV };
}

/** Query options shared by every step query (pcb's queryOptions, with
 *  `settingSources: []` as settled for BubblePaw). */
export function queryOptions(base: {
	cwd: string;
	model: string;
	systemPrompt: string;
	env?: Record<string, string | undefined>;
	systemPromptMode?: SystemPromptMode;
}) {
	return {
		cwd: base.cwd,
		env: childEnv(base.env ?? process.env),
		model: base.model,
		tools: [] as string[],
		settingSources: [] as string[],
		strictMcpConfig: true,
		permissionMode: "bypassPermissions",
		allowDangerouslySkipPermissions: true,
		includePartialMessages: true,
		settings: {
			autoMemoryEnabled: false,
			claudeMdExcludes: CLAUDE_MD_EXCLUDES,
			// The gitStatus block is the trailing suffix of the cached system block;
			// a git-state change would bust the cache for the whole conversation (pcb).
			includeGitInstructions: false,
		},
		systemPrompt: base.systemPromptMode === "replace"
			? base.systemPrompt
			: { type: "preset", preset: "claude_code", append: base.systemPrompt || undefined },
	};
}

type Block =
	| { type: "text"; text: string; native?: any; index?: number }
	| { type: "tool"; id: string; name: string; partialJson: string; args: unknown; native?: any; index?: number }
	| { type: "opaque"; native: any; index?: number };

interface MessageUsage {
	input: number;
	output: number;
	cacheRead: number;
	cacheWrite: number;
	reasoning?: number;
}

/** Per-step turn state (pcb's per-turn QueryContext fields). */
class Turn {
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
function reportedFailure(error: ProviderError, turn: Turn): ProviderError {
	if (error.usage === undefined && turn.usage.size > 0) error.usage = turn.totals();
	if (error.reportedModel === undefined) error.reportedModel = turn.reportedModel();
	return error;
}

interface Waiter {
	stepId: unknown;
	onText: (text: string) => void;
	resolve: (response: ModelResponse) => void;
	reject: (error: ProviderError) => void;
}

/** A running Claude Code query (pcb's QueryContext). */
export class Live {
	quotaEvents: RateLimitInfo[] = [];
	alive = true;
	ccSessionId: string | undefined;
	waiter: Waiter | null = null;
	turn = new Turn();
	failure: FailureContext = {};
	readonly lot = new ParkingLot();
	readonly served: Set<string>;
	handle: ReturnType<QueryFn> | null = null;
	promptStream: PromptStream | null = null;
	/** Set when we killed it on purpose; its late events and errors are ignored. */
	discarded = false;
	/** API message ids earlier steps already returned. The SDK repeats a streamed
	 *  message as an `assistant` record after message_stop; one arriving after the
	 *  next step began must not be taken for new content. */
	readonly doneMessageIds = new Set<string>();
	readonly returnedUsage: Usage = { input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0 };

	constructor(
		readonly sessionKey: string,
		readonly model: string,
		readonly toolNames: string[],
		resumeId: string | null,
		readonly requestOptions: RequestOptions,
	) {
		this.served = new Set(toolNames);
		this.ccSessionId = resumeId ?? undefined;
	}

	kill(reason: string): void {
		this.discarded = true;
		this.alive = false;
		this.promptStream?.fail(new Error(reason));
		// Settle parked handlers before killing the CLI: one left awaiting a dead
		// process never settles (pcb's drainForAbort).
		this.lot.releaseAll(reason);
		const handle = this.handle;
		void handle?.interrupt?.().catch(() => {});
		try {
			handle?.close?.();
		} catch {}
	}
}

export interface StepLog {
	session: string | null;
	path: "fresh" | "deliver" | "reuse" | "rebuild" | "one-off";
	reason?: string;
	cc_session?: string;
	usage?: ReturnType<Turn["totals"]>;
	/** The full model ID the SDK reported, which names the usage row. */
	reported_model?: string;
	tool_calls?: number;
	/** User messages pushed into the running query this step. */
	steered?: number;
	error?: { kind: string; message: string };
}

export interface ProviderOptions {
	query?: QueryFn;
	cwd?: string;
	claudeDir?: string;
	env?: Record<string, string | undefined>;
	systemPromptMode?: SystemPromptMode;
	/** Called once per finished step (diagnostics, the live-run report). */
	onStep?: (entry: StepLog) => void;
	log?: (line: string) => void;
	/** Writes rebuilt session files; replaceable in tests. */
	writeSession?: typeof writeSession;
}

export class ClaudeCodeProvider {
	readonly mirrors = new Map<string, Mirror>();
	readonly lives = new Map<string, Live>();
	/** Running step id -> the live query serving it, for `cancel`. */
	private readonly running = new Map<unknown, { live: Live | null; cancel: () => void }>();
	private readonly query: QueryFn;
	private readonly cwd: string;
	private readonly write: typeof writeSession;

	constructor(private readonly opts: ProviderOptions = {}) {
		this.query = opts.query ?? (sdkQuery as unknown as QueryFn);
		this.cwd = opts.cwd ?? process.cwd();
		this.write = opts.writeSession ?? writeSession;
	}

	private log(line: string): void {
		this.opts.log?.(line);
	}

	async step(request: ModelRequest, stepId: unknown, onText: (text: string) => void): Promise<ModelResponse> {
		const options = requestOptions(request);
		if (!request.session_id) return this.oneOff(request, stepId, onText, options);
		const key = request.session_id;
		const mirror = this.mirrors.get(key);
		const current = this.lives.get(key);
		const decision = decide(
			mirror,
			request,
			current && { alive: current.alive, steerable: current.alive && current.promptStream !== null, model: current.model, toolNames: current.toolNames },
		);

		if (decision.kind === "deliver" && current) {
			assertUnchangedOptions(current.requestOptions, options);
			const steered = decision.steer.map((m) => m.id);
			const reason = steered.length ? `steered ${steered.length}` : undefined;
			return this.track(stepId, current, key, request, "deliver", reason, async () => {
				current.turn = new Turn();
				current.turn.steered = steered;
				const promise = this.await(current, stepId, onText);
				// Steering (pcb's deliverToolResults): push the user messages with
				// priority "next" and wait until each is written to Claude Code's
				// stdin, then release the tool results over the same FIFO. Claude
				// Code drains the queued messages at this tool boundary, so its turn
				// holds the tool results followed by the user text, as the store does.
				for (const message of decision.steer) {
					try {
						await current.promptStream!.push(userMessage([{ type: "text", text: message.content || "[empty]" }], "next"));
					} catch (error) {
						current.waiter = null;
						throw new ProviderError("transient", "Claude Code steering failed");
					}
					this.log(`steer ${message.id}`);
				}
				// Ephemeral context rides on the tool-result turn, after the last result.
				const notes = notesText(decision.notes);
				decision.results.forEach((result, index) => {
					const content = [{ type: "text" as const, text: result.content ?? "" }];
					if (notes && index === decision.results.length - 1) content.push({ type: "text", text: notes });
					const outcome = current.lot.deliver(result.tool_call_id!, { content, isError: Boolean(result.is_error) });
					this.log(`deliver ${result.tool_call_id} ${outcome}`);
				});
				return promise;
			});
		}

		// Anything else starts a fresh query. A query still parked here belongs to a
		// conversation the request no longer continues: kill it and rotate the
		// session id, since it may still flush records into the old file (pcb's
		// discardRewrittenQuery).
		let forceRotate = mirror?.forceRotate ?? false;
		if (current) {
			this.lives.delete(key);
			current.kill("This query was discarded; the conversation is being rebuilt.");
			forceRotate = true;
		}

		let resumeId: string | null;
		let prompt: string;
		let path: StepLog["path"];
		let reason: string | undefined;
		if (decision.kind === "reuse" && mirror) {
			resumeId = mirror.ccSessionId;
			prompt = decision.prompt;
			path = "reuse";
		} else {
			const rebuild = decision as Extract<Decision, { kind: "rebuild" }>;
			// The old mirror is invalid from here; it is replaced once this step succeeds.
			this.mirrors.delete(key);
			const written = this.write({
				priors: rebuild.priors,
				cwd: this.cwd,
				claudeDir: this.opts.claudeDir,
				previousId: mirror?.ccSessionId,
				forceRotate,
				model: request.model,
			});
			for (const warning of written.warnings) this.log(`rebuild warning: ${warning}`);
			resumeId = written.sessionId;
			prompt = rebuild.prompt;
			path = resumeId ? "rebuild" : "fresh";
			reason = rebuild.reason;
			if (resumeId) this.log(`rebuild ${resumeId} from ${rebuild.priors.length} messages (${written.records} records): ${rebuild.reason}`);
		}

		const live = new Live(key, request.model, request.tools.map((t) => t.name), resumeId, options);
		this.lives.set(key, live);
		return this.track(stepId, live, key, request, path, reason, () => {
			const promise = this.await(live, stepId, onText);
			this.start(live, request, prompt, resumeId);
			return promise;
		});
	}

	/** Run one step against `live`, then record the mirror from its outcome. */
	private async track(
		stepId: unknown,
		live: Live,
		key: string,
		request: ModelRequest,
		path: StepLog["path"],
		reason: string | undefined,
		run: () => Promise<ModelResponse>,
	): Promise<ModelResponse> {
		let cancelledByHost = false;
		this.running.set(stepId, {
			live,
			cancel: () => {
				cancelledByHost = true;
				const waiter = live.waiter;
				live.waiter = null;
				waiter?.reject(cancelled());
			},
		});
		try {
			const response = await run();
			const toolUseIds = response.tool_calls.map((c) => c.id);
			const ccSessionId = live.ccSessionId;
			if (ccSessionId) {
				this.mirrors.set(key, {
					ccSessionId,
					epoch: request.history?.epoch ?? "",
					// Only stable messages count; ephemeral ones are not history.
					cursor: stableCount(request.messages),
					lastStep: { toolUseIds },
					model: request.model,
					needsRebuild: false,
					forceRotate: false,
					cwd: this.cwd,
				});
			}
			this.opts.onStep?.({ session: key, path, reason, cc_session: ccSessionId, usage: live.turn.totals(), ...(response.reported_model ? { reported_model: response.reported_model } : {}), tool_calls: toolUseIds.length, ...(live.turn.steered.length ? { steered: live.turn.steered.length } : {}) });
			return response;
		} catch (error) {
			const failure = reportedFailure(classifyThrown(error, live.failure), live.turn);
			const state = this.mirrors.get(key);
			if (cancelledByHost || failure.message === "cancelled") {
				// Interrupt the query; the killed process may still flush a late record,
				// so the next sync rebuilds under a fresh id (pcb's abort handling).
				if (this.lives.get(key) === live) this.lives.delete(key);
				live.kill("cancelled");
				if (state) this.mirrors.set(key, { ...state, needsRebuild: true, forceRotate: true });
			} else {
				// The conversation is in an unknown state after an error: drop the mirror (pcb).
				if (this.lives.get(key) === live) this.lives.delete(key);
				live.kill("query failed");
				this.mirrors.delete(key);
			}
			this.opts.onStep?.({ session: key, path, reason, cc_session: live.ccSessionId, usage: live.turn.totals(), ...(failure.reportedModel ? { reported_model: failure.reportedModel } : {}), error: { kind: failure.kind, message: failure.message } });
			throw failure;
		} finally {
			this.running.delete(stepId);
		}
	}

	private await(live: Live, stepId: unknown, onText: (text: string) => void): Promise<ModelResponse> {
		return new Promise<ModelResponse>((resolve, reject) => {
			live.waiter = { stepId, onText, resolve, reject };
		});
	}

	private start(live: Live, request: ModelRequest, prompt: string, resumeId: string | null): void {
		const promptStream = makePromptStream();
		void promptStream.push(userMessage([{ type: "text", text: prompt }])).catch(() => {});
		live.promptStream = promptStream;
		const mcpServers = request.tools.length
			? { [MCP_SERVER_NAME]: createToolServer(MCP_SERVER_NAME, request.tools, live.lot) }
			: undefined;
		const options: any = {
			...queryOptions({
				cwd: this.cwd,
				model: request.model,
				systemPrompt: request.system_prompt,
				env: this.opts.env,
				systemPromptMode: this.opts.systemPromptMode,
			}),
			...live.requestOptions.native,
			...(mcpServers ? { mcpServers } : {}),
			...(resumeId ? { resume: resumeId } : {}),
			stderr: () => this.log("claude: process diagnostic received"),
		};
		let handle: ReturnType<QueryFn>;
		try {
			handle = this.query({ prompt: promptStream.stream, options });
		} catch (error) {
			this.fail(live, classifyThrown(error));
			return;
		}
		live.handle = handle;
		void this.consume(live, handle);
	}

	/** pcb's consumeQuery: iterate the SDK messages, routing content to the step
	 *  waiting on this query. */
	private async consume(live: Live, handle: ReturnType<QueryFn>): Promise<void> {
		try {
			for await (const message of handle) {
				if (live.discarded) break;
				if (typeof message?.session_id === "string") live.ccSessionId = message.session_id;
				if (message.type === "result") {
					live.promptStream?.end();
					live.alive = false;
					const errorText = resultErrorText(message);
					// A failed query still reports what it used.
					live.turn.resultUsage(message.usage, live.returnedUsage, message.modelUsage);
					if (errorText !== undefined) {
						const detail = live.turn.syntheticText ? `${errorText}\n${live.turn.syntheticText}` : errorText;
						this.fail(live, classify(detail, live.failure));
					} else {
						this.resultOutput(live, message);
						if (!live.turn.sawStream && !live.turn.text() && typeof message.result === "string" && message.result) {
							live.turn.blocks.push({ type: "text", text: message.result });
							live.waiter?.onText(message.result);
						}
						this.finish(live);
					}
					if (this.lives.get(live.sessionKey) === live) this.lives.delete(live.sessionKey);
					continue;
				}
				if (message.type === "rate_limit_event") {
					const info = rateLimitInfo(message.rate_limit_info);
					if (info) live.quotaEvents.push(info);
					if (info?.status === "rejected") live.failure.rateLimitRejection = info;
					continue;
				}
				if (message.type === "assistant" && message.error) live.failure.assistantError = message.error;
				if (!live.waiter) continue;
				if (message.type === "stream_event") this.streamEvent(live, message.event);
				else if (message.type === "assistant") this.assistantMessage(live, message.message);
			}
			live.alive = false;
			if (live.waiter && !live.discarded) this.fail(live, new ProviderError("transient", "Claude Code query ended without a result"));
		} catch (error) {
			live.alive = false;
			if (!live.discarded) this.fail(live, classifyThrown(error, live.failure));
		} finally {
			live.alive = false;
			live.promptStream?.fail(new Error("query ended"));
			live.lot.releaseAll("Query ended");
			if (this.lives.get(live.sessionKey) === live) this.lives.delete(live.sessionKey);
			try {
				handle.close?.();
			} catch {}
		}
	}

	private finish(live: Live): void {
		const waiter = live.waiter;
		if (!waiter) return;
		live.waiter = null;
		for (const id of [...live.turn.usage.keys(), live.turn.streamMessageId, live.turn.lastMessageId]) if (id) live.doneMessageIds.add(id);
		const toolCalls = live.turn.toolCalls();
		const totals = live.turn.totals();
		live.returnedUsage.input_tokens += totals.input_tokens;
		live.returnedUsage.output_tokens += totals.output_tokens;
		live.returnedUsage.cache_read_tokens = (live.returnedUsage.cache_read_tokens ?? 0) + (totals.cache_read_tokens ?? 0);
		live.returnedUsage.cache_creation_tokens = (live.returnedUsage.cache_creation_tokens ?? 0) + (totals.cache_creation_tokens ?? 0);
		if (totals.reasoning_tokens !== undefined) live.returnedUsage.reasoning_tokens = (live.returnedUsage.reasoning_tokens ?? 0) + totals.reasoning_tokens;
		const reported = live.turn.reportedModel();
		waiter.resolve({
			text: live.turn.text(),
			tool_calls: toolCalls,
			usage: totals,
			...(reported ? { reported_model: reported } : {}),
			...(live.quotaEvents.length ? { quota_events: live.quotaEvents.splice(0) } : {}),
			provider_state: {
				...replayState(live.turn.content(), live.turn.text(), toolCalls, live.model),
				claude_code: {
					session_id: live.ccSessionId ?? "",
					tool_use_ids: toolCalls.map((c) => c.id),
					...(live.turn.lastMessageId ? { message_id: live.turn.lastMessageId } : {}),
					...(live.turn.steered.length ? { steered: live.turn.steered } : {}),
				},
			},
		});
	}

	private fail(live: Live, error: ProviderError): void {
		const waiter = live.waiter;
		if (!waiter) return;
		live.waiter = null;
		reportedFailure(error, live.turn);
		error.quotaEvents.push(...live.quotaEvents.splice(0));
		waiter.reject(error);
	}

	private resultOutput(live: Live, message: any): void {
		if (live.requestOptions.schema !== undefined && message.structured_output === undefined) {
			throw new ProviderError("fatal", "output_schema requested but Claude Code returned no structured_output");
		}
		if (message.structured_output !== undefined) {
			live.turn.structuredOutput(message.structured_output);
			live.waiter?.onText(live.turn.text());
		}
	}

	private emitText(live: Live, text: string): void {
		// The SDK may narrate its internal formatting tool. Only native structured
		// output is published for a schema request; opaque blocks still accumulate.
		if (live.requestOptions.schema === undefined) live.waiter?.onText(text);
	}

	/** pcb's processStreamEvent. */
	private streamEvent(live: Live, event: any): void {
		const turn = live.turn;
		turn.sawStream = true;
		switch (event?.type) {
			case "message_start": {
				// Still open from an earlier message_start: Claude Code gave up on that
				// stream and is retrying it; its blocks were never completed.
				if (turn.streamOpen) turn.dropAbandoned();
				turn.streamMessageId = event.message?.id;
				turn.lastMessageId = event.message?.id;
				turn.streamOpen = true;
				turn.blockStart = turn.blocks.length;
				turn.recordUsage(event.message?.id, event.message?.usage, event.message?.model);
				return;
			}
			case "content_block_start": {
				const block = event.content_block;
				if (block?.type === "text") {
					turn.blocks.push({ type: "text", text: block.text ?? "", native: structuredClone(block), index: event.index });
				} else if (block?.type === "tool_use") {
					const name = bubblepawToolName(block.name, live.served);
					if (!name) {
						this.log(`skipping tool_use for unserved tool ${block.name} [${block.id}]; Claude Code rejects it and retries`);
						return;
					}
					turn.blocks.push({ type: "tool", id: block.id, name, partialJson: "", args: block.input === undefined ? {} : block.input, native: structuredClone(block), index: event.index });
				} else if (block && typeof block.type === "string") {
					turn.blocks.push({ type: "opaque", native: structuredClone(block), index: event.index });
				}
				return;
			}
			case "content_block_delta": {
				const block = turn.blocks.find((b) => b.index === event.index);
				if (!block) return;
				if (event.delta?.type === "text_delta" && block.type === "text") {
					block.text += event.delta.text;
					this.emitText(live, event.delta.text);
				} else if (event.delta?.type === "input_json_delta" && block.type === "tool") {
					block.partialJson += event.delta.partial_json;
				} else if (block.type === "opaque" && event.delta?.type === "thinking_delta") {
					block.native.thinking = (block.native.thinking ?? "") + event.delta.thinking;
				} else if (block.type === "opaque" && event.delta?.type === "signature_delta") {
					block.native.signature = (block.native.signature ?? "") + event.delta.signature;
				}
				return;
			}
			case "content_block_stop": {
				const block = turn.blocks.find((b) => b.index === event.index);
				if (!block) return;
				delete block.index;
				if (block.type === "tool" && block.partialJson) {
					try {
						block.args = JSON.parse(block.partialJson);
					} catch {
						// Every served MCP tool has a root object schema. Keep the call
						// metered, but null guarantees rejection at host validation.
						block.args = null;
						this.log(`unparseable tool input for ${block.id}`);
					}
				} else if (block.type === "tool" && block.native?.input === undefined) {
					block.args = null;
				}
				return;
			}
			case "message_delta":
				turn.recordUsage(turn.streamMessageId, event.usage);
				return;
			case "message_stop":
				turn.streamOpen = false;
				// A tool block without content_block_stop was cut off. Preserve its
				// identity for metering, but never return its provisional {} input.
				for (const block of turn.blocks) if (block.type === "tool" && block.index !== undefined) {
					block.args = null;
					delete block.index;
				}
				// Tool calls complete: end the step. The MCP handlers park Claude
				// Code until the next step delivers the results.
				if (turn.toolCalls().length > 0) this.finish(live);
				return;
		}
	}

	/** pcb's processAssistantMessage: the content path when no stream events
	 *  delivered this message (Claude Code's non-streaming fallback for a stalled
	 *  stream), and the carrier of `<synthetic>` failure reports. */
	private assistantMessage(live: Live, message: any): void {
		const turn = live.turn;
		if (!message?.content) return;
		if (message.id && live.doneMessageIds.has(message.id)) return;
		if (message.model === "<synthetic>") {
			if (turn.sawStream && turn.streamOpen) turn.dropAbandoned();
			for (const block of message.content) if (block.type === "text" && block.text) turn.syntheticText += block.text;
			return;
		}
		if (turn.sawStream) {
			const id = message.id;
			if (!id || !turn.streamMessageId) return;
			if (id === turn.streamMessageId) {
				turn.recordUsage(id, message.usage, message.model);
				return;
			}
			if (turn.streamOpen) turn.dropAbandoned();
		}
		turn.lastMessageId = message.id ?? turn.lastMessageId;
		let sawTool = false;
		for (const block of message.content) {
			if (block.type === "text" && block.text) {
				turn.blocks.push({ type: "text", text: block.text, native: structuredClone(block) });
				this.emitText(live, block.text);
			} else if (block.type === "tool_use") {
				const name = bubblepawToolName(block.name, live.served);
				if (!name) continue;
				sawTool = true;
				turn.blocks.push({ type: "tool", id: block.id, name, partialJson: "", args: block.input === undefined ? null : block.input, native: structuredClone(block) });
			} else if (typeof block.type === "string") {
				turn.blocks.push({ type: "opaque", native: structuredClone(block) });
			}
		}
		turn.recordUsage(message.id, message.usage, message.model);
		if (sawTool) this.finish(live);
	}

	/** Session-less steps (summaries): pcb's isolated summary, a separate
	 *  `persistSession: false` query with the request's prompt as a custom system
	 *  prompt, one turn and no tools. */
	private async oneOff(request: ModelRequest, stepId: unknown, onText: (text: string) => void, mapped: RequestOptions): Promise<ModelResponse> {
		const options: any = {
			...queryOptions({ cwd: this.cwd, model: request.model, systemPrompt: "", env: this.opts.env }),
			...mapped.native,
			systemPrompt: request.system_prompt,
			persistSession: false,
			maxTurns: 1,
			stderr: () => this.log("claude: process diagnostic received"),
		};
		delete options.permissionMode;
		delete options.allowDangerouslySkipPermissions;
		let handle: ReturnType<QueryFn>;
		try {
			handle = this.query({ prompt: oneOffPrompt(request.messages), options });
		} catch (error) {
			throw classifyThrown(error);
		}
		let aborted = false;
		this.running.set(stepId, {
			live: null,
			cancel: () => {
				aborted = true;
				void handle.interrupt?.().catch(() => {});
				try {
					handle.close?.();
				} catch {}
			},
		});
		const failure: FailureContext = {};
		const live = new Live("", request.model, [], null, mapped);
		live.waiter = { stepId, onText, resolve: () => {}, reject: () => {} };
		let usage: Usage | null = null;
		try {
			for await (const message of handle) {
				if (aborted) break;
				if (message.type === "rate_limit_event") {
					const info = rateLimitInfo(message.rate_limit_info);
					if (info) live.quotaEvents.push(info);
					if (info?.status === "rejected") failure.rateLimitRejection = info;
				}
				if (message.type === "assistant") {
					if (message.error) failure.assistantError = message.error;
					this.assistantMessage(live, message.message);
				} else if (message.type === "stream_event") {
					this.streamEvent(live, message.event);
				} else if (message.type === "result") {
					// A failed one-off still reports what it used.
					live.turn.resultModels = message.modelUsage;
					if (message.usage) aggregateOneOff(live.turn, message.usage);
					const errorText = resultErrorText(message);
					if (errorText !== undefined) throw classify(errorText, failure);
					this.resultOutput(live, message);
					if (!live.turn.text() && typeof message.result === "string" && message.result) {
						live.turn.blocks.push({ type: "text", text: message.result });
						this.emitText(live, message.result);
					}
					usage = live.turn.totals();
				}
			}
			if (aborted) throw cancelled();
			const reported = live.turn.reportedModel();
			this.opts.onStep?.({ session: null, path: "one-off", usage: usage ?? live.turn.totals(), ...(reported ? { reported_model: reported } : {}) });
			const text = live.turn.text();
			return { text, tool_calls: [], usage, ...(reported ? { reported_model: reported } : {}), ...(live.quotaEvents.length ? { quota_events: live.quotaEvents } : {}),
				provider_state: live.turn.blocks.length ? replayState(live.turn.content(), text, [], request.model) : undefined };
		} catch (error) {
			const typed = reportedFailure(aborted ? cancelled() : classifyThrown(error, failure), live.turn);
			typed.quotaEvents.push(...live.quotaEvents);
			throw typed;
		} finally {
			this.running.delete(stepId);
			try {
				handle.close?.();
			} catch {}
		}
	}

	/** `cancel`: interrupt the running query; the step fails `transient`/`cancelled`. */
	cancel(stepId: unknown): void {
		this.running.get(stepId)?.cancel();
	}

	/** `session.close`: end the query and drop the mirror. Claude Code's files stay. */
	closeSession(sessionId: string): void {
		const live = this.lives.get(sessionId);
		if (live) {
			this.lives.delete(sessionId);
			const waiter = live.waiter;
			live.waiter = null;
			waiter?.reject(new ProviderError("transient", "session closed"));
			live.kill("session closed");
		}
		this.mirrors.delete(sessionId);
	}
}

/** A one-off result is the aggregate for its whole isolated query: it replaces the
 *  per-message totals and keeps the model they named, plus the thinking delta. */
function aggregateOneOff(turn: Turn, usage: any): void {
	const reasoning = turn.totals().reasoning_tokens;
	const model = turn.reportedModel();
	turn.usage.clear();
	turn.messageModels.clear();
	turn.recordUsage(undefined, usage, model);
	if (usage.output_tokens_details?.thinking_tokens == null && reasoning !== undefined) {
		turn.recordUsage(undefined, { output_tokens_details: { thinking_tokens: reasoning } });
	}
}

/** Append one JSON line per step to `path` (BUBBLEPAW_CC_LOG). */
export function fileStepLogger(path: string): (entry: StepLog) => void {
	return (entry) => {
		try {
			appendFileSync(path, `${JSON.stringify({ at: new Date().toISOString(), ...entry })}\n`);
		} catch {}
	};
}
