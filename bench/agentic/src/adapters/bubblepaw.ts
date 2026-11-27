import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { estimateCostUsd } from "../pricing.ts";
import type { AdapterConfig, AgentAdapter, AgentRunRequest, AgentRunResult } from "../types.ts";

interface BubblePawEvent {
  type?: string;
  session_id?: string;
  text?: string;
  call?: { id?: string; name?: string };
  result?: { is_error?: boolean };
  run?: {
    status?: string;
    text?: string | null;
    error?: string | null;
    usage?: {
      input_tokens?: number;
      output_tokens?: number;
      cache_read_tokens?: number;
      cache_creation_tokens?: number;
      reasoning_tokens?: number | null;
    } | null;
  };
}

interface TraceEnvelope {
  record?: {
    kind?: string;
    metrics?: Record<string, unknown>;
  };
}

interface UsageExportRow {
  input_tokens?: number;
  cache_read_tokens?: number;
  cache_creation_tokens?: number;
  output_tokens?: number;
  reasoning_tokens?: number | null;
  cost_usd?: number | null;
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

async function parseTraceFile(tracePath: string): Promise<{
  firstTextMs: number | null;
  unexplainedCacheMisses: number;
  repeatedToolCalls: number;
  traceToolFailed: number;
}> {
  let raw: string;
  try {
    raw = await readFile(tracePath, "utf8");
  } catch {
    return {
      firstTextMs: null,
      unexplainedCacheMisses: 0,
      repeatedToolCalls: 0,
      traceToolFailed: 0,
    };
  }

  let firstTextMs: number | null = null;
  let explicitMissEvents = 0;
  let fallbackMissFromFinished = 0;
  let traceToolFailed = 0;
  const fingerprints = new Map<string, number>();

  for (const rawLine of raw.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) {
      continue;
    }
    let env: TraceEnvelope;
    try {
      env = JSON.parse(line) as TraceEnvelope;
    } catch {
      continue;
    }
    const kind = env.record?.kind;
    const metrics = env.record?.metrics;
    if (!kind || !metrics) {
      continue;
    }
    if (kind === "model_finished") {
      if (firstTextMs === null && typeof metrics.first_text_ms === "number") {
        firstTextMs = metrics.first_text_ms;
      }
      // Same rule as BubblePaw's own warning (docs/prefix-cache-design.md): a miss counts only if
      // at least 1024 tokens were expected cached and the shortfall exceeds max(128, 10%).
      // A bare unexplained_miss_tokens > 0 is provider cache-block rounding (e.g. 64 tokens).
      const expected = metrics.expected_cacheable_tokens;
      const measured = metrics.measured_cache_read_tokens;
      if (
        typeof expected === "number" &&
        typeof measured === "number" &&
        expected >= 1024 &&
        expected - measured > Math.max(128, expected * 0.1)
      ) {
        fallbackMissFromFinished += 1;
      }
    } else if (kind === "cache_miss_unexplained") {
      explicitMissEvents += 1;
    } else if (kind === "cache_bust" && metrics.declared === false) {
      explicitMissEvents += 1;
    } else if (kind === "tool_started") {
      const tool = typeof metrics.tool === "string" ? metrics.tool : "unknown";
      const fp = typeof metrics.input_fingerprint === "string" ? metrics.input_fingerprint : "";
      const key = `${tool}:${fp}`;
      fingerprints.set(key, (fingerprints.get(key) ?? 0) + 1);
    } else if (kind === "tool_failed") {
      traceToolFailed += 1;
    }
  }

  let repeatedToolCalls = 0;
  for (const count of fingerprints.values()) {
    if (count > 1) {
      repeatedToolCalls += count - 1;
    }
  }

  return {
    firstTextMs,
    unexplainedCacheMisses:
      explicitMissEvents > 0 ? explicitMissEvents : fallbackMissFromFinished,
    repeatedToolCalls,
    traceToolFailed,
  };
}

async function queryUsageExport(
  bin: string,
  workspace: string,
  dataDir: string,
  configPath: string,
  sessionId: string | null,
): Promise<{
  requests: number;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  cacheCreationTokens: number;
  reasoningTokens: number | null;
  costUsd: number | null;
} | null> {
  const args = [
    bin,
    "--workspace",
    workspace,
    "--data-dir",
    dataDir,
    "--config",
    configPath,
    "usage",
    "export",
  ];
  if (sessionId) {
    // Filtering by root session keeps main + summary/decider rows tied to that session if needed;
    // since dataDir is per-run fresh, omit --session to also capture any child session rows.
  }

  let proc: ReturnType<typeof Bun.spawn>;
  try {
    proc = Bun.spawn(args, {
      cwd: workspace,
      stdout: "pipe",
      stderr: "pipe",
    });
  } catch {
    return null;
  }

  const timer = setTimeout(() => {
    try {
      proc.kill();
    } catch {
      // ignore
    }
  }, 10_000);

  const [stdout, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    proc.exited,
  ]);
  clearTimeout(timer);

  if (exitCode !== 0 || stdout.trim().length === 0) {
    return null;
  }

  let requests = 0;
  let inputTokens = 0;
  let outputTokens = 0;
  let cachedTokens = 0;
  let cacheCreationTokens = 0;
  let reasoningTokens: number | null = null;
  let costSum = 0;
  let hasUnpriced = false;

  for (const rawLine of stdout.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) {
      continue;
    }
    let row: UsageExportRow;
    try {
      row = JSON.parse(line) as UsageExportRow;
    } catch {
      continue;
    }
    requests += 1;
    inputTokens += row.input_tokens ?? 0;
    outputTokens += row.output_tokens ?? 0;
    cachedTokens += row.cache_read_tokens ?? 0;
    cacheCreationTokens += row.cache_creation_tokens ?? 0;
    if (typeof row.reasoning_tokens === "number") {
      reasoningTokens = (reasoningTokens ?? 0) + row.reasoning_tokens;
    }
    if (typeof row.cost_usd === "number") {
      costSum += row.cost_usd;
    } else {
      hasUnpriced = true;
    }
  }

  if (requests === 0) {
    return null;
  }

  return {
    requests,
    inputTokens,
    outputTokens,
    cachedTokens,
    cacheCreationTokens,
    reasoningTokens,
    costUsd: hasUnpriced ? null : Number(costSum.toFixed(9)),
  };
}

export class BubblePawAdapter implements AgentAdapter {
  readonly name = "bubblepaw";
  private readonly bin: string;
  private readonly configPath: string;
  private readonly maxRounds?: number;

  constructor(config: AdapterConfig = {}) {
    this.bin = config.bin ?? process.env.BUBBLEPAW_BIN ?? "bubblepaw";
    const here = dirname(fileURLToPath(import.meta.url));
    this.configPath = config.bubblepawConfig ?? resolve(here, "../../bubblepaw.toml");
    this.maxRounds = config.maxRounds;
  }

  async version(): Promise<string | null> {
    try {
      const proc = Bun.spawn([this.bin, "--version"], {
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
    const dataDir = join(req.scratchDir, "data");
    const traceFile = join(req.scratchDir, "trace.jsonl");
    const maxRounds = req.maxRounds ?? this.maxRounds;

    const args = [
      this.bin,
      "--workspace",
      req.workspace,
      "--data-dir",
      dataDir,
      "--config",
      this.configPath,
      "--model",
      req.model,
      "--trace-file",
      traceFile,
      "--trace-mode",
      "metadata",
      "--json",
    ];
    if (typeof maxRounds === "number" && maxRounds > 0) {
      args.push("--set", `agent.max_rounds=${maxRounds}`);
    }
    args.push("run", "--prompt-file", req.promptFile);

    const startedAt = performance.now();
    const state: {
      firstTextMs: number | null;
      streamRequests: number;
      streamToolCalls: number;
      streamToolErrors: number;
      resultEvent: BubblePawEvent | null;
      paragraphs: string[];
    } = {
      firstTextMs: null,
      streamRequests: 0,
      streamToolCalls: 0,
      streamToolErrors: 0,
      resultEvent: null,
      paragraphs: [],
    };

    let proc: ReturnType<typeof Bun.spawn>;
    try {
      proc = Bun.spawn(args, {
        cwd: req.workspace,
        stdout: "pipe",
        stderr: "pipe",
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
        let ev: BubblePawEvent;
        try {
          ev = JSON.parse(line) as BubblePawEvent;
        } catch {
          return;
        }
        if (ev.type === "text_delta" || ev.type === "paragraph") {
          if (state.firstTextMs === null && typeof ev.text === "string" && ev.text.length > 0) {
            state.firstTextMs = Math.round(performance.now() - startedAt);
          }
          if (ev.type === "paragraph" && typeof ev.text === "string") {
            state.paragraphs.push(ev.text);
          }
        } else if (ev.type === "model_request_started") {
          state.streamRequests += 1;
        } else if (ev.type === "tool_started") {
          state.streamToolCalls += 1;
        } else if (ev.type === "tool_finished" && ev.result?.is_error === true) {
          state.streamToolErrors += 1;
        } else if (ev.type === "result") {
          state.resultEvent = ev;
        }
      }),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    clearTimeout(timer);

    const wallMs = Math.round(performance.now() - startedAt);
    const traceSummary = await parseTraceFile(traceFile);
    if (state.firstTextMs === null) {
      state.firstTextMs = traceSummary.firstTextMs;
    }

    const sessionId = state.resultEvent?.session_id ?? null;
    const usageExport = await queryUsageExport(
      this.bin,
      req.workspace,
      dataDir,
      this.configPath,
      sessionId,
    );

    const runUsage = state.resultEvent?.run?.usage ?? null;
    const inputTokens = usageExport ? usageExport.inputTokens : (runUsage?.input_tokens ?? null);
    const outputTokens = usageExport ? usageExport.outputTokens : (runUsage?.output_tokens ?? null);
    const cachedTokens = usageExport
      ? usageExport.cachedTokens
      : (runUsage?.cache_read_tokens ?? null);
    const cacheCreationTokens = usageExport
      ? usageExport.cacheCreationTokens
      : (runUsage?.cache_creation_tokens ?? null);
    const reasoningTokens = usageExport
      ? usageExport.reasoningTokens
      : (runUsage?.reasoning_tokens ?? null);

    let costUsd = usageExport?.costUsd ?? null;
    let costSource: "agent" | "catalog" | null = costUsd !== null ? "agent" : null;
    if (costUsd === null && inputTokens !== null && outputTokens !== null) {
      const estimated = estimateCostUsd(req.model, {
        inputTokens,
        outputTokens,
        cachedTokens,
        cacheCreationTokens,
      });
      if (estimated !== null) {
        costUsd = estimated;
        costSource = "catalog";
      }
    }

    if (req.artifactDir) {
      await mkdir(req.artifactDir, { recursive: true });
      await writeFile(join(req.artifactDir, "stdout.jsonl"), stdoutRaw, "utf8");
      await writeFile(join(req.artifactDir, "stderr.log"), stderrRaw, "utf8");
      try {
        await copyFile(traceFile, join(req.artifactDir, "trace.jsonl"));
      } catch {
        // trace file may not exist if spawn failed early
      }
    }

    const finalText =
      typeof state.resultEvent?.run?.text === "string"
        ? state.resultEvent.run.text
        : state.paragraphs.join("\n\n");

    let error: string | null = null;
    if (timedOut) {
      error = `Timed out after ${req.timeoutMs}ms`;
    } else if (state.resultEvent?.run?.status && state.resultEvent.run.status !== "completed") {
      error = state.resultEvent.run.error ?? `run status: ${state.resultEvent.run.status}`;
    } else if (exitCode !== 0) {
      const firstLine = stderrRaw.trim().split(/\r?\n/)[0]?.trim();
      error = firstLine && firstLine.length > 0 ? firstLine : `Exited with code ${exitCode}`;
    }

    return {
      finalText,
      wallMs,
      firstTextMs: state.firstTextMs,
      requests: usageExport
        ? usageExport.requests
        : state.streamRequests > 0
          ? state.streamRequests
          : null,
      inputTokens,
      outputTokens,
      cachedTokens,
      cacheCreationTokens,
      reasoningTokens,
      costUsd,
      costSource,
      toolCalls: state.streamToolCalls,
      toolErrors: Math.max(state.streamToolErrors, traceSummary.traceToolFailed),
      exitCode,
      error,
      timedOut,
      unexplainedCacheMisses: traceSummary.unexplainedCacheMisses,
      repeatedToolCalls: traceSummary.repeatedToolCalls,
    };
  }
}
