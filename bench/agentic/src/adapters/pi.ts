import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { estimateCostUsd } from "../pricing.ts";
import type { AdapterConfig, AgentAdapter, AgentRunRequest, AgentRunResult } from "../types.ts";

interface PiUsageShape {
  input?: number;
  output?: number;
  cacheRead?: number;
  cacheWrite?: number;
  input_tokens?: number;
  output_tokens?: number;
  cache_read_tokens?: number;
  cache_creation_tokens?: number;
  reasoning?: number;
  reasoning_tokens?: number;
  cost?: { total?: number; totalFromProvider?: boolean } | number;
  cost_usd?: number;
}

interface PiContentBlock {
  type?: string;
  text?: string;
  name?: string;
  arguments?: unknown;
}

interface PiCompactionResult {
  usage?: PiUsageShape;
}

interface PiMessage {
  role?: string;
  content?: PiContentBlock[] | string;
  usage?: PiUsageShape;
  stopReason?: string;
  errorMessage?: string;
}

interface PiEvent {
  type?: string;
  delta?: string;
  assistantMessageEvent?: {
    type?: string;
    delta?: string;
  };
  message?: PiMessage;
  messages?: PiMessage[];
  result?: PiCompactionResult;
  aborted?: boolean;
  toolName?: string;
  args?: unknown;
  isError?: boolean;
  is_error?: boolean;
}

async function readStreamLines(
  stream: ReadableStream<Uint8Array> | null,
  onLine: (line: string) => void,
): Promise<string> {
  if (!stream) {
    return "";
  }
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let full = "";
  let carry = "";
  while (true) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    const chunk = decoder.decode(value, { stream: true });
    full += chunk;
    carry += chunk;
    let idx = carry.indexOf("\n");
    while (idx >= 0) {
      const line = carry.slice(0, idx).trim();
      carry = carry.slice(idx + 1);
      if (line.length > 0) {
        onLine(line);
      }
      idx = carry.indexOf("\n");
    }
  }
  const tail = decoder.decode();
  if (tail.length > 0) {
    full += tail;
    carry += tail;
  }
  if (carry.trim().length > 0) {
    onLine(carry.trim());
  }
  return full;
}

function extractMessageText(message: PiMessage | undefined): string {
  if (!message) {
    return "";
  }
  if (typeof message.content === "string") {
    return message.content;
  }
  if (!Array.isArray(message.content)) {
    return "";
  }
  return message.content
    .filter((block) => block.type === "text" && typeof block.text === "string")
    .map((block) => block.text ?? "")
    .join("");
}

function extractUsageCost(usage: PiUsageShape): number | null {
  if (typeof usage.cost_usd === "number") {
    return usage.cost_usd;
  }
  if (typeof usage.cost === "number") {
    return usage.cost;
  }
  if (usage.cost && typeof usage.cost.total === "number") {
    return usage.cost.total;
  }
  return null;
}

function resolvePiProviderAndModel(
  model: string,
  config: AdapterConfig,
): { provider: string; modelId: string } {
  if (config.piProvider && config.piModel) {
    return { provider: config.piProvider, modelId: config.piModel };
  }
  const slash = model.indexOf("/");
  if (slash > 0) {
    const rawProvider = model.slice(0, slash);
    const modelId = config.piModel ?? model.slice(slash + 1);
    const provider =
      config.piProvider ?? (rawProvider === "opencode" ? "opencode-go" : rawProvider);
    return { provider, modelId };
  }
  return {
    provider: config.piProvider ?? "opencode-go",
    modelId: config.piModel ?? model,
  };
}

/**
 * The files of a per-run pi agent directory (PI_CODING_AGENT_DIR) that give the model a context
 * window of `tokens`: models.json overrides the built-in model's window, settings.json keeps
 * automatic compaction on (pi compacts when context > window - compaction.reserveTokens, 16384 by
 * default). The user's own ~/.pi/agent is never read or written; the API key comes from the
 * environment (OPENCODE_API_KEY).
 */
export function piContextWindowFiles(
  provider: string,
  modelId: string,
  tokens: number,
): { "settings.json": string; "models.json": string } {
  return {
    "settings.json": JSON.stringify({ compaction: { enabled: true } }, null, 2),
    "models.json": JSON.stringify(
      { providers: { [provider]: { modelOverrides: { [modelId]: { contextWindow: tokens } } } } },
      null,
      2,
    ),
  };
}

export class PiAdapter implements AgentAdapter {
  readonly name = "pi";
  private readonly bin: string;
  private readonly node: string | undefined;
  private readonly config: AdapterConfig;

  constructor(config: AdapterConfig = {}) {
    this.bin = config.bin ?? process.env.PI_BIN ?? "pi";
    // Optional Node executable that runs `bin` (a JS entry such as pi's dist/bundle/cli.js).
    // Lets pi run on a portable Node without touching the system Node or PATH.
    this.node = config.piNode ?? process.env.PI_NODE ?? undefined;
    this.config = config;
  }

  private command(args: string[]): string[] {
    return this.node ? [this.node, this.bin, ...args] : [this.bin, ...args];
  }

  async version(): Promise<string | null> {
    try {
      const proc = Bun.spawn(this.command(["--version"]), {
        stdout: "pipe",
        stderr: "pipe",
      });
      const [stdout, exitCode] = await Promise.all([
        new Response(proc.stdout).text(),
        proc.exited,
      ]);
      if (exitCode !== 0) {
        return null;
      }
      const first = stdout.trim().split(/\r?\n/)[0]?.trim();
      return first && first.length > 0 ? first : null;
    } catch {
      return null;
    }
  }

  async run(req: AgentRunRequest): Promise<AgentRunResult> {
    const { provider, modelId } = resolvePiProviderAndModel(req.model, this.config);
    const tools = this.config.piTools ?? "read,bash,edit,write,grep,find,ls";
    // Thinking level matches bubblepaw.toml (reasoning_effort = "low"); pi's own default is "medium".
    const thinking = this.config.piThinking ?? "low";
    const args = this.command([
      "--mode",
      "json",
      "-p",
      "--no-session",
      "-nc",
      "-ne",
      "-ns",
      "-np",
      "--offline",
      "--tools",
      tools,
      "--provider",
      provider,
      "--model",
      modelId,
      "--thinking",
      thinking,
      ...(this.config.piExtraArgs ?? []),
      req.prompt,
    ]);

    const startedAt = performance.now();
    const state: {
      firstTextMs: number | null;
      turnStarts: number;
      assistantMessages: number;
      compactions: number;
      compactionRequests: number;
      peakInputTokens: number;
      toolStartEvents: number;
      toolErrors: number;
      inputTokens: number | null;
      outputTokens: number | null;
      cachedTokens: number | null;
      cacheCreationTokens: number | null;
      reasoningTokens: number | null;
      reportedCostSum: number;
      sawReportedCost: boolean;
      costFromPiCatalog: boolean;
      lastAssistantText: string;
      lastAssistantError: string | null;
      agentEndMessages: PiMessage[] | null;
    } = {
      firstTextMs: null,
      turnStarts: 0,
      assistantMessages: 0,
      compactions: 0,
      compactionRequests: 0,
      peakInputTokens: 0,
      toolStartEvents: 0,
      toolErrors: 0,
      inputTokens: null,
      outputTokens: null,
      cachedTokens: null,
      cacheCreationTokens: null,
      reasoningTokens: null,
      reportedCostSum: 0,
      sawReportedCost: false,
      costFromPiCatalog: false,
      lastAssistantText: "",
      lastAssistantError: null,
      agentEndMessages: null,
    };
    const toolFingerprints = new Map<string, number>();

    const accumulateUsage = (usage: PiUsageShape | undefined): void => {
      if (!usage) {
        return;
      }
      const inp = usage.input ?? usage.input_tokens;
      const out = usage.output ?? usage.output_tokens;
      const cacheRead = usage.cacheRead ?? usage.cache_read_tokens;
      const cacheWrite = usage.cacheWrite ?? usage.cache_creation_tokens;
      const reason = usage.reasoning ?? usage.reasoning_tokens;
      if (typeof inp === "number") {
        state.peakInputTokens = Math.max(
          state.peakInputTokens,
          inp +
            (typeof cacheRead === "number" ? cacheRead : 0) +
            (typeof cacheWrite === "number" ? cacheWrite : 0),
        );
        // pi's `input` EXCLUDES cache reads and writes (pi-ai: input = prompt_tokens - cacheRead -
        // cacheWrite). Normalise to the whole prompt, as BubblePaw's input_tokens already is.
        state.inputTokens =
          (state.inputTokens ?? 0) +
          inp +
          (typeof cacheRead === "number" ? cacheRead : 0) +
          (typeof cacheWrite === "number" ? cacheWrite : 0);
      }
      if (typeof out === "number") {
        state.outputTokens = (state.outputTokens ?? 0) + out;
      }
      if (typeof cacheRead === "number") {
        state.cachedTokens = (state.cachedTokens ?? 0) + cacheRead;
      }
      if (typeof cacheWrite === "number") {
        state.cacheCreationTokens = (state.cacheCreationTokens ?? 0) + cacheWrite;
      }
      if (typeof reason === "number") {
        state.reasoningTokens = (state.reasoningTokens ?? 0) + reason;
      }
      if (
        typeof usage.cost === "object" &&
        usage.cost !== null &&
        usage.cost.totalFromProvider === false
      ) {
        state.costFromPiCatalog = true;
      }
      const c = extractUsageCost(usage);
      if (typeof c === "number") {
        state.reportedCostSum += c;
        state.sawReportedCost = true;
      }
    };

    let env: Record<string, string | undefined> = process.env;
    if (typeof req.contextWindow === "number" && req.contextWindow > 0) {
      // A per-run agent directory: the context window and compaction settings for this run only.
      const agentDir = join(req.scratchDir, "pi-agent");
      await mkdir(agentDir, { recursive: true });
      const files = piContextWindowFiles(provider, modelId, req.contextWindow);
      for (const [name, body] of Object.entries(files)) {
        await writeFile(join(agentDir, name), body, "utf8");
      }
      env = { ...process.env, PI_CODING_AGENT_DIR: agentDir };
    }

    let proc: ReturnType<typeof Bun.spawn>;
    try {
      proc = Bun.spawn(args, {
        cwd: req.workspace,
        stdout: "pipe",
        stderr: "pipe",
        env,
      });
    } catch (err) {
      const wallMs = Math.round(performance.now() - startedAt);
      const message = err instanceof Error ? err.message : String(err);
      return {
        finalText: "",
        wallMs,
        firstTextMs: null,
        requests: null,
        inputTokens: null,
        outputTokens: null,
        cachedTokens: null,
        cacheCreationTokens: null,
        reasoningTokens: null,
        costUsd: null,
        costSource: null,
        toolCalls: null,
        toolErrors: null,
        exitCode: null,
        error: `Failed to spawn ${this.bin}: ${message}`,
        timedOut: false,
        unexplainedCacheMisses: null,
        repeatedToolCalls: null,
      };
    }

    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      try {
        proc.kill();
      } catch {
        // ignore kill race
      }
    }, req.timeoutMs);

    const [stdoutRaw, stderrRaw, exitCode] = await Promise.all([
      readStreamLines(proc.stdout, (line) => {
        let ev: PiEvent;
        try {
          ev = JSON.parse(line) as PiEvent;
        } catch {
          return;
        }
        if (ev.type === "turn_start") {
          state.turnStarts += 1;
        } else if (ev.type === "message_update") {
          const ame = ev.assistantMessageEvent;
          if (
            state.firstTextMs === null &&
            ame?.type === "text_delta" &&
            typeof ame.delta === "string" &&
            ame.delta.length > 0
          ) {
            state.firstTextMs = Math.round(performance.now() - startedAt);
          }
        } else if (ev.type === "text_delta") {
          if (state.firstTextMs === null && typeof ev.delta === "string" && ev.delta.length > 0) {
            state.firstTextMs = Math.round(performance.now() - startedAt);
          }
        } else if (ev.type === "tool_execution_start") {
          state.toolStartEvents += 1;
          const fp = `${ev.toolName ?? "unknown"}:${JSON.stringify(ev.args ?? {})}`;
          toolFingerprints.set(fp, (toolFingerprints.get(fp) ?? 0) + 1);
        } else if (ev.type === "tool_execution_end") {
          if (ev.isError === true || ev.is_error === true) {
            state.toolErrors += 1;
          }
        } else if (ev.type === "message_end" && ev.message?.role === "assistant") {
          state.assistantMessages += 1;
          accumulateUsage(ev.message.usage);
          const text = extractMessageText(ev.message);
          if (text.length > 0) {
            state.lastAssistantText = text;
            if (state.firstTextMs === null) {
              state.firstTextMs = Math.round(performance.now() - startedAt);
            }
          }
          if (ev.message.stopReason === "error" && ev.message.errorMessage) {
            state.lastAssistantError = ev.message.errorMessage;
          }
        } else if (ev.type === "compaction_end" && ev.result && ev.aborted !== true) {
          // pi's summary request is a model request too (BubblePaw counts its summary rows).
          state.compactions += 1;
          if (ev.result.usage) {
            state.compactionRequests += 1;
            accumulateUsage(ev.result.usage);
          }
        } else if (ev.type === "agent_end" && Array.isArray(ev.messages)) {
          state.agentEndMessages = ev.messages;
        }
      }),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    clearTimeout(timer);

    const wallMs = Math.round(performance.now() - startedAt);

    // Fallback to agent_end.messages only if message_end events were not seen.
    if (state.assistantMessages === 0 && state.agentEndMessages) {
      for (const msg of state.agentEndMessages) {
        if (msg.role === "assistant") {
          state.assistantMessages += 1;
          accumulateUsage(msg.usage);
          const text = extractMessageText(msg);
          if (text.length > 0) {
            state.lastAssistantText = text;
          }
          if (msg.stopReason === "error" && msg.errorMessage) {
            state.lastAssistantError = msg.errorMessage;
          }
          if (state.toolStartEvents === 0 && Array.isArray(msg.content)) {
            for (const block of msg.content) {
              if (block.type === "toolCall") {
                state.toolStartEvents += 1;
                const fp = `${block.name ?? "unknown"}:${JSON.stringify(block.arguments ?? {})}`;
                toolFingerprints.set(fp, (toolFingerprints.get(fp) ?? 0) + 1);
              }
            }
          }
        }
      }
    }

    if (req.artifactDir) {
      await mkdir(req.artifactDir, { recursive: true });
      await writeFile(join(req.artifactDir, "stdout.jsonl"), stdoutRaw, "utf8");
      await writeFile(join(req.artifactDir, "stderr.log"), stderrRaw, "utf8");
    }

    let costUsd: number | null =
      state.sawReportedCost && state.reportedCostSum > 0
        ? Number(state.reportedCostSum.toFixed(9))
        : null;
    // pi prices tokens from its own model catalog unless the provider reports a cost.
    let costSource: "agent" | "catalog" | null =
      costUsd !== null ? (state.costFromPiCatalog ? "catalog" : "agent") : null;
    if (costUsd === null && state.inputTokens !== null && state.outputTokens !== null) {
      const estimated = estimateCostUsd(req.model, {
        inputTokens: state.inputTokens,
        outputTokens: state.outputTokens,
        cachedTokens: state.cachedTokens,
        cacheCreationTokens: state.cacheCreationTokens,
      });
      if (estimated !== null) {
        costUsd = estimated;
        costSource = "catalog";
      }
    }

    let repeatedToolCalls = 0;
    for (const count of toolFingerprints.values()) {
      if (count > 1) {
        repeatedToolCalls += count - 1;
      }
    }

    const requests =
      state.assistantMessages > 0
        ? state.assistantMessages + state.compactionRequests
        : state.turnStarts > 0
          ? state.turnStarts
          : null;

    let error: string | null = null;
    if (timedOut) {
      error = `Timed out after ${req.timeoutMs}ms`;
    } else if (state.lastAssistantError) {
      error = state.lastAssistantError;
    } else if (exitCode !== 0) {
      const firstLine = stderrRaw.trim().split(/\r?\n/)[0]?.trim();
      error = firstLine && firstLine.length > 0 ? firstLine : `Exited with code ${exitCode}`;
    }

    return {
      finalText: state.lastAssistantText,
      wallMs,
      firstTextMs: state.firstTextMs,
      requests,
      inputTokens: state.inputTokens,
      outputTokens: state.outputTokens,
      cachedTokens: state.cachedTokens,
      cacheCreationTokens: state.cacheCreationTokens,
      reasoningTokens: state.reasoningTokens,
      costUsd,
      costSource,
      toolCalls: state.toolStartEvents,
      toolErrors: state.toolErrors,
      exitCode,
      error,
      timedOut,
      unexplainedCacheMisses: null,
      repeatedToolCalls,
      compactions: state.compactions,
      peakRequestInputTokens: state.peakInputTokens > 0 ? state.peakInputTokens : null,
    };
  }
}
