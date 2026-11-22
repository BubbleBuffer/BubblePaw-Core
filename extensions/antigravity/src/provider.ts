// The step engine: drives Antigravity (`agy acp`) one model round at a time.
//
// Mechanism (the same park/deliver shape as the Claude Code extension): one agy
// session per BubblePaw session, one ACP `session/prompt` per run of rounds.
// BubblePaw's tools reach agy as an MCP server named `bubblepaw` (mcp-shim.ts,
// connected back through bridge.ts). When agy calls one, the call is PARKED and the
// step returns it as a tool call; Rust runs the tool (approvals, effects and storage
// stay BubblePaw's) and the next step carries the result as a tool message, which
// releases the parked call so the same agy turn goes on. A turn that ends without
// tool calls returns its text as the step's final reply.
//
// BubblePaw owns the history (mirror.ts decides append versus reseed); agy's own
// built-in tools are denied twice: permission requests (policy.ts) and a PreToolUse
// hook (hook.ts, installed in the extension's own workspace).

import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { AcpProcess } from "./acp.js";
import { Bridge } from "./bridge.js";
import { cancelled, classifyThrown } from "./errors.js";
import { configKey, decide, notesText, type Mirror } from "./mirror.js";
import { resolveModel } from "./models.js";
import { hooksConfig, MCP_SERVER_NAME, MCP_TOOL_PREFIX, permissionOutcome } from "./policy.js";
import type { RpcId } from "./rpc.js";
import { forkPrompt, isEphemeral, seedPrompt } from "./seed.js";
import { ProviderError, type McpResult, type ModelRequest, type ModelResponse, type ToolCall, type Usage } from "./types.js";

/** Environment variables that could switch agy off its cached login (an API key,
 *  Vertex/ADC credentials). Dropped from the child unless the instance's `env`
 *  table names them (the host lists those in BUBBLEPAW_EXTENSION_ENV_KEYS). */
export const STRIPPED_ENV = [
	"GEMINI_API_KEY",
	"GOOGLE_API_KEY",
	"GOOGLE_GENAI_USE_VERTEXAI",
	"GOOGLE_GENAI_USE_GCA",
	"GOOGLE_APPLICATION_CREDENTIALS",
	"GOOGLE_CLOUD_PROJECT",
	"GOOGLE_CLOUD_LOCATION",
	"AGY_ADC_AUTH",
];

export function childEnv(base: Record<string, string | undefined>, keep: readonly string[] = []): Record<string, string> {
	const out: Record<string, string> = {};
	for (const [k, v] of Object.entries(base)) {
		if (v === undefined) continue;
		if (STRIPPED_ENV.includes(k) && !keep.includes(k)) continue;
		out[k] = v;
	}
	return out;
}

const TOOLS_NOTE = `Use only the \`${MCP_SERVER_NAME}\` tools (the tools whose names start with mcp_${MCP_SERVER_NAME}_). Every other built-in tool is disabled in this environment.`;
const NO_TOOLS_NOTE = "No tools are available in this environment: answer in text only.";

export interface StepLog {
	session: string;
	kind: "step" | "fork";
	model: string;
	toolCalls: number;
	usage: Usage | null;
}

export interface ProviderOptions {
	command: string;
	args: string[];
	/** agy's workspace: a directory the extension owns, never the user's project. */
	cwd: string;
	env: Record<string, string>;
	/** The MCP shim agy is told to spawn. */
	shim: { command: string; args: string[] };
	/** When set, written to `<cwd>/.agents/hooks.json` as the PreToolUse hook command. */
	hookCommand?: string;
	/** Quiet period after a tool call parks before the step returns, so parallel calls travel together. */
	settleMs?: number;
	log: (line: string) => void;
	onStep?: (entry: StepLog) => void;
}

interface Parked {
	id: string;
	name: string;
	args: unknown;
	resolve: (result: McpResult) => void;
	/** Retries of this call by the model (agy times a call out and the model asks again), answered with the same result. */
	retries: Array<(result: McpResult) => void>;
}

/** Give a parked call and its retries the one result. */
function answer(call: Parked, result: McpResult): void {
	call.resolve(result);
	for (const retry of call.retries) retry(result);
}

/** Same arguments, whatever the key order. */
function canonical(value: unknown): string {
	if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
	if (value && typeof value === "object") return `{${Object.entries(value).sort(([a], [b]) => (a < b ? -1 : 1)).map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(",")}}`;
	return JSON.stringify(value) ?? "null";
}

interface Turn {
	done: boolean;
	res?: any;
	err?: unknown;
	/** Text streamed since the last step returned. */
	text: string;
	/** Calls parked since the last step returned. */
	fresh: Parked[];
	settled: boolean;
	timer?: ReturnType<typeof setTimeout>;
	wake?: () => void;
	cancelled: boolean;
	/** The agy process died under the turn. */
	died?: string;
}

class Sess {
	mirror!: Mirror;
	token = "";
	tools: ModelRequest["tools"] = [];
	turn?: Turn;
	readonly parked = new Map<string, Parked>();
	onText?: (text: string) => void;
	seq = 0;
	base = "";
	usage: Usage | null = null;
}

export class AntigravityProvider {
	/** BubblePaw session id -> its agy session. */
	readonly lives = new Map<string, Sess>();
	private readonly byAcp = new Map<string, Sess>();
	private readonly active = new Map<RpcId, Sess>();
	private readonly bridge = new Bridge();
	private acp?: AcpProcess;
	private acpReady?: Promise<AcpProcess>;
	private readonly settleMs: number;

	constructor(private readonly opts: ProviderOptions) {
		this.settleMs = opts.settleMs ?? 300;
	}

	// ---- process and workspace -------------------------------------------------

	private prepareWorkspace(): void {
		mkdirSync(this.opts.cwd, { recursive: true });
		const hooks = join(this.opts.cwd, ".agents", "hooks.json");
		if (!this.opts.hookCommand) return rmSync(hooks, { force: true }); // the workspace is ours: no hook means no stale hook
		mkdirSync(join(this.opts.cwd, ".agents"), { recursive: true });
		writeFileSync(hooks, `${JSON.stringify(hooksConfig(this.opts.hookCommand), null, 2)}\n`);
	}

	private ensureAcp(): Promise<AcpProcess> {
		if (this.acp?.alive && this.acpReady) return this.acpReady;
		this.prepareWorkspace();
		const proc = new AcpProcess({
			command: this.opts.command,
			args: this.opts.args,
			env: this.opts.env,
			cwd: this.opts.cwd,
			log: this.opts.log,
			onRequest: (method, params) => this.onAgentRequest(method, params),
			onNotify: (method, params) => this.onUpdate(method, params),
			onExit: (reason) => this.onExit(proc, reason),
		});
		this.acp = proc;
		this.acpReady = (async () => {
			await this.bridge.start();
			await proc.request("initialize", { protocolVersion: 1, clientCapabilities: {}, clientInfo: { name: "bubblepaw-antigravity", version: "0.1.0" } });
			return proc;
		})();
		this.acpReady.catch(() => {});
		return this.acpReady;
	}

	private onExit(proc: AcpProcess, reason: string): void {
		if (this.acp !== proc) return;
		this.acp = undefined;
		this.acpReady = undefined;
		for (const sess of this.lives.values()) {
			if (sess.turn && !sess.turn.done) {
				sess.turn.died = reason;
				sess.turn.wake?.();
			}
			this.releaseAll(sess, `agy ${reason}`);
		}
		this.lives.clear();
		this.byAcp.clear();
		this.opts.log(`agy ${reason}`);
	}

	private onAgentRequest(method: string, params: any): unknown {
		if (method === "session/request_permission") {
			const outcome = permissionOutcome(params);
			this.opts.log(`permission ${String(params?.toolCall?.name)}: ${"optionId" in outcome.outcome ? (String(params?.toolCall?.name).startsWith(MCP_TOOL_PREFIX) ? "allowed" : "rejected") : "cancelled"}`);
			return outcome;
		}
		throw new Error(`unsupported client method ${method}`);
	}

	private onUpdate(method: string, params: any): void {
		if (method !== "session/update") return;
		const sess = this.byAcp.get(params?.sessionId);
		const update = params?.update;
		if (!sess?.turn || update?.sessionUpdate !== "agent_message_chunk") return;
		const text = update.content?.type === "text" ? String(update.content.text ?? "") : "";
		if (!text) return;
		sess.turn.text += text;
		sess.onText?.(text);
	}

	// ---- parking ---------------------------------------------------------------

	private park(sess: Sess, name: string, args: unknown): Promise<McpResult> {
		const turn = sess.turn;
		if (!turn || turn.done || turn.cancelled) return Promise.resolve({ content: [{ type: "text", text: "This tool call is no longer wanted." }], isError: true });
		const id = `agy_${Date.now().toString(36)}_${++sess.seq}`;
		return new Promise<McpResult>((resolve) => {
			// agy gives up on a call after about 180 s and the model may ask again. While the original is still
			// outstanding the retry is the same call: it waits for the original's result and never reaches the host.
			const key = canonical(args);
			for (const original of sess.parked.values()) {
				if (original.name === name && canonical(original.args) === key) {
					original.retries.push(resolve);
					this.opts.log(`retry of ${name} attached to ${original.id}`);
					return;
				}
			}
			const call: Parked = { id, name, args, resolve, retries: [] };
			sess.parked.set(id, call);
			turn.fresh.push(call);
			if (turn.timer) clearTimeout(turn.timer);
			turn.timer = setTimeout(() => {
				turn.timer = undefined;
				turn.settled = true;
				turn.wake?.();
			}, this.settleMs);
		});
	}

	/** Answer every parked call with `reason`. Resolved, never rejected: an error
	 *  reply is still a reply, and a call left waiting keeps agy's tools/call open. */
	private releaseAll(sess: Sess, reason: string): void {
		for (const call of sess.parked.values()) answer(call, { content: [{ type: "text", text: reason }], isError: true });
		sess.parked.clear();
	}

	// ---- sessions --------------------------------------------------------------

	private async open(request: ModelRequest, sid: string, withTools: boolean): Promise<Sess> {
		const proc = await this.ensureAcp();
		const sess = new Sess();
		sess.tools = withTools ? request.tools : [];
		const mcpServers = [];
		if (sess.tools.length > 0) {
			sess.token = this.bridge.register({
				tools: () => sess.tools,
				call: (name, args) => this.park(sess, name, args),
			});
			mcpServers.push({
				name: MCP_SERVER_NAME,
				command: this.opts.shim.command,
				args: this.opts.shim.args,
				env: [
					{ name: "BUBBLEPAW_AGY_PIPE", value: this.bridge.path },
					{ name: "BUBBLEPAW_AGY_TOKEN", value: sess.token },
				],
			});
		}
		const append = `${request.system_prompt}\n\n${sess.tools.length > 0 ? TOOLS_NOTE : NO_TOOLS_NOTE}`.trim();
		let created: any;
		try {
			created = await proc.request("session/new", { cwd: this.opts.cwd, mcpServers, _meta: { antigravity: { systemPrompt: { append } } } });
			const available: string[] = (created?.models?.availableModels ?? []).map((m: any) => String(m.modelId));
			const model = resolveModel(available, request.model, request.reasoning_effort);
			sess.base = model.base;
			await proc.request("session/set_model", { sessionId: created.sessionId, modelId: model.id });
		} catch (error) {
			if (sess.token) this.bridge.unregister(sess.token);
			if (created?.sessionId) this.closeAcp(created.sessionId);
			throw error;
		}
		sess.mirror = { acpSessionId: created.sessionId, epoch: request.history?.epoch, configKey: configKey(request), cursor: 0, lastCallIds: [], turnAlive: false };
		this.byAcp.set(created.sessionId, sess);
		if (withTools) this.lives.set(sid, sess);
		return sess;
	}

	private closeAcp(acpSessionId: string): void {
		if (!this.acp?.alive) return;
		this.acp.request("session/close", { sessionId: acpSessionId }).catch(() => {});
	}

	/** Forget a session: release its parked calls, close its agy session. */
	private drop(sess: Sess, sid?: string): void {
		this.releaseAll(sess, "The session was reset.");
		if (sess.token) this.bridge.unregister(sess.token);
		if (sid !== undefined && this.lives.get(sid) === sess) this.lives.delete(sid);
		if (sess.mirror) {
			this.byAcp.delete(sess.mirror.acpSessionId);
			this.closeAcp(sess.mirror.acpSessionId);
		}
		sess.mirror.turnAlive = false;
	}

	closeSession(sid: string): void {
		const sess = this.lives.get(sid);
		if (sess) this.drop(sess, sid);
	}

	shutdown(): void {
		for (const sid of [...this.lives.keys()]) this.closeSession(sid);
		this.acp?.kill();
		this.bridge.close();
	}

	// ---- turns -----------------------------------------------------------------

	private startTurn(sess: Sess, prompt: string): void {
		const turn: Turn = { done: false, text: "", fresh: [], settled: false, cancelled: false };
		sess.turn = turn;
		this.acp!.request("session/prompt", { sessionId: sess.mirror.acpSessionId, prompt: [{ type: "text", text: prompt }] }).then(
			(res) => {
				this.opts.log(`agy turn ended (${res?.stopReason}) with ${sess.parked.size} parked call(s)`);
				turn.done = true;
				turn.res = res;
				turn.wake?.();
			},
			(err) => {
				turn.done = true;
				turn.err = err;
				turn.wake?.();
			},
		);
	}

	private awaitRound(turn: Turn): Promise<void> {
		return new Promise((resolve) => {
			turn.wake = resolve;
			if (turn.done || turn.settled) resolve();
		});
	}

	/** Wait until the turn either ends or has parked calls, then build the reply. */
	private async round(sess: Sess, id: RpcId, onText: (text: string) => void, request: ModelRequest, cursor: number): Promise<ModelResponse> {
		const turn = sess.turn!;
		sess.onText = onText;
		this.active.set(id, sess);
		if (turn.text) onText(turn.text); // text streamed before this step attached (rare)
		try {
			await this.awaitRound(turn);
		} finally {
			this.active.delete(id);
			sess.onText = undefined;
		}
		const text = turn.text;
		turn.text = "";
		if (turn.cancelled) throw cancelled();
		if (turn.died) throw new ProviderError("transient", `Antigravity transport failed: agy ${turn.died}`);
		if (turn.done) {
			sess.turn = undefined;
			sess.mirror.turnAlive = false;
			if (turn.err) throw classifyThrown(turn.err);
			if (turn.res?.stopReason === "cancelled") throw cancelled();
			const usage = toUsage(turn.res?.usage);
			sess.mirror.cursor = cursor;
			sess.mirror.lastCallIds = [];
			return this.response(sess, text, [], usage, request);
		}
		const calls = turn.fresh;
		turn.fresh = [];
		turn.settled = false;
		sess.mirror.cursor = cursor;
		sess.mirror.lastCallIds = calls.map((c) => c.id);
		sess.mirror.turnAlive = true;
		return this.response(sess, text, calls.map((c): ToolCall => ({ id: c.id, name: c.name, arguments: c.args ?? {} })), null, request);
	}

	private response(sess: Sess, text: string, tool_calls: ToolCall[], usage: Usage | null, request: ModelRequest): ModelResponse {
		this.opts.onStep?.({ session: request.session_id ?? "", kind: "step", model: sess.base, toolCalls: tool_calls.length, usage });
		return {
			text,
			tool_calls,
			usage,
			provider_state: { antigravity: { session_id: sess.mirror.acpSessionId, call_ids: tool_calls.map((c) => c.id) } },
			reported_model: sess.base || undefined,
		};
	}

	/** One model round. */
	async step(request: ModelRequest, id: RpcId, onText: (text: string) => void): Promise<ModelResponse> {
		const sid = request.session_id ?? "";
		const prior = this.lives.get(sid);
		// A turn that ended while its calls were parked (agy gave up on them) cannot take their results.
		if (prior?.turn?.done) prior.mirror.turnAlive = false;
		const decision = decide(prior?.mirror, request);
		const cursor = request.messages.filter((m) => !isEphemeral(m)).length;
		let sess: Sess;
		try {
			if (decision.kind === "deliver" && prior) {
				sess = prior;
				const notes = [notesText(decision.notes), ...decision.steer.map((m) => m.content)].filter((t) => t).join("\n\n");
				const last = decision.results.length - 1;
				const turn = sess.turn!;
				turn.fresh = [];
				turn.settled = false;
				decision.results.forEach((m, i) => {
					const body = i === last && notes ? `${m.content}\n\n<system-note>\n${notes}\n</system-note>` : m.content;
					const call = sess.parked.get(m.tool_call_id ?? "");
					sess.parked.delete(m.tool_call_id ?? "");
					if (call) answer(call, { content: [{ type: "text", text: body }], isError: Boolean(m.is_error) });
				});
				sess.mirror.lastCallIds = [];
			} else if (decision.kind === "reuse" && prior) {
				sess = prior;
				this.startTurn(sess, decision.prompt);
			} else {
				if (prior) this.drop(prior, sid);
				this.opts.log(`session ${sid}: seeding a new agy session (${decision.kind === "reseed" ? decision.reason : "reseed"})`);
				sess = await this.open(request, sid, true);
				this.startTurn(sess, seedPrompt(request.messages));
			}
		} catch (error) {
			throw classifyOpen(error);
		}
		try {
			return await this.round(sess, id, onText, request, cursor);
		} catch (error) {
			this.drop(sess, sid);
			throw classifyOpen(error);
		}
	}

	/** `summary_fork`: a fresh agy session without tools, the fork's messages as the
	 *  seed, one prompt; the session is closed afterwards. */
	async summaryFork(request: ModelRequest, id: RpcId, onText: (text: string) => void): Promise<ModelResponse> {
		let sess: Sess;
		try {
			sess = await this.open(request, "", false);
			this.startTurn(sess, forkPrompt(request.messages));
		} catch (error) {
			throw classifyOpen(error);
		}
		try {
			const response = await this.round(sess, id, onText, request, 0);
			if (response.tool_calls.length > 0) throw new ProviderError("fatal", "the summary fork called a tool; none was run");
			return { ...response, provider_state: undefined };
		} catch (error) {
			throw classifyOpen(error);
		} finally {
			this.drop(sess);
		}
	}

	/** `cancel`: stop the running step, release its parked calls. The agy session is
	 *  dropped, so the next step reseeds; a parked call is never replayed. */
	cancel(id: unknown): void {
		const sess = this.active.get(id as RpcId);
		const turn = sess?.turn;
		if (!sess || !turn) return;
		turn.cancelled = true;
		if (this.acp?.alive) this.acp.notify("session/cancel", { sessionId: sess.mirror.acpSessionId });
		this.releaseAll(sess, "cancelled");
		turn.wake?.();
	}
}

function classifyOpen(error: unknown): ProviderError {
	if (error instanceof ProviderError) return error;
	const text = error instanceof Error ? error.message : String(error);
	if (/ENOENT|not found/i.test(text) && /spawn/i.test(text)) return new ProviderError("fatal", "Antigravity CLI `agy` was not found: install it or set BUBBLEPAW_AGY_BIN");
	return classifyThrown(error);
}

function toUsage(u: any): Usage | null {
	if (!u) return null;
	return {
		input_tokens: u.inputTokens ?? 0,
		output_tokens: u.outputTokens ?? 0,
		cache_read_tokens: u.cachedReadTokens ?? 0,
		cache_creation_tokens: u.cachedWriteTokens ?? 0,
		reasoning_tokens: u.thoughtTokens ?? 0,
	};
}

