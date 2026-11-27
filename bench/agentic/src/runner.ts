import { appendFile, cp, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { createAdapter } from "./adapters/index.ts";
import { countChangedFiles } from "./check-helpers.ts";
import { defaultTasksDir, discoverTasks, prepareWorkspace, runTaskCheck } from "./tasks.ts";
import type {
  AgentAdapter,
  BenchmarkResult,
  RunRecord,
  RunnerOptions,
  TaskDefinition,
} from "./types.ts";

export const DEFAULT_MODEL = "opencode/deepseek-v4.1-flash";
export const DEFAULT_TIMEOUT_MS = 180_000;

interface ScheduledRun {
  task: TaskDefinition;
  agentName: string;
  rep: number;
}

function seededShuffle<T>(items: T[], seed: number): T[] {
  const arr = [...items];
  let state = seed >>> 0 || 1;
  for (let i = arr.length - 1; i > 0; i -= 1) {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    const j = state % (i + 1);
    const tmp = arr[i]!;
    arr[i] = arr[j]!;
    arr[j] = tmp;
  }
  return arr;
}

function buildSchedule(
  tasks: TaskDefinition[],
  agents: string[],
  reps: number,
  interleave: boolean,
  seedOrder?: number,
): ScheduledRun[] {
  const schedule: ScheduledRun[] = [];
  if (!interleave) {
    for (const agentName of agents) {
      for (let rep = 1; rep <= reps; rep += 1) {
        for (const task of tasks) {
          schedule.push({ task, agentName, rep });
        }
      }
    }
    return schedule;
  }

  for (let rep = 1; rep <= reps; rep += 1) {
    const repBatch: ScheduledRun[] = [];
    for (let t = 0; t < tasks.length; t += 1) {
      const task = tasks[t]!;
      // Rotate agent order per task and rep so both agents share API load conditions symmetrically.
      for (let a = 0; a < agents.length; a += 1) {
        const agentIdx = (a + t + rep - 1) % agents.length;
        repBatch.push({ task, agentName: agents[agentIdx]!, rep });
      }
    }
    if (seedOrder !== undefined) {
      schedule.push(...seededShuffle(repBatch, seedOrder + rep));
    } else {
      schedule.push(...repBatch);
    }
  }
  return schedule;
}

export async function runSingleExecution(params: {
  task: TaskDefinition;
  adapter: AgentAdapter;
  agentVersion: string | null;
  model: string;
  rep: number;
  timeoutMs: number;
  maxRounds?: number;
  keepWorkspacesDir?: string;
}): Promise<RunRecord> {
  const { task, adapter, agentVersion, model, rep, timeoutMs, maxRounds, keepWorkspacesDir } =
    params;

  const tempRoot = await mkdtemp(join(tmpdir(), "bp-bench-"));
  const workspaceDir = join(tempRoot, "workspace");
  const scratchDir = join(tempRoot, "scratch");
  await mkdir(workspaceDir, { recursive: true });
  await mkdir(scratchDir, { recursive: true });

  const promptFile = join(scratchDir, "prompt.md");
  await writeFile(promptFile, task.prompt, "utf8");

  const artifactDir = keepWorkspacesDir
    ? join(keepWorkspacesDir, `${task.meta.id}__${adapter.name}__rep${rep}`, "artifacts")
    : undefined;

  try {
    const beforeHashes = await prepareWorkspace(task.seedDir, workspaceDir);

    const effectiveTimeout = task.meta.timeout_ms ?? timeoutMs;
    const agentRes = await adapter.run({
      taskId: task.meta.id,
      prompt: task.prompt,
      promptFile,
      workspace: workspaceDir,
      scratchDir,
      model,
      timeoutMs: effectiveTimeout,
      maxRounds,
      artifactDir,
    });

    const filesChanged = await countChangedFiles(beforeHashes, workspaceDir);
    const checkRes = await runTaskCheck(task, workspaceDir, agentRes.finalText, scratchDir);

    if (keepWorkspacesDir) {
      const dest = join(keepWorkspacesDir, `${task.meta.id}__${adapter.name}__rep${rep}`);
      await mkdir(dest, { recursive: true });
      await cp(workspaceDir, join(dest, "workspace"), { recursive: true, force: true });
    }

    const cacheHitRatio =
      agentRes.cachedTokens !== null &&
      agentRes.inputTokens !== null &&
      agentRes.inputTokens > 0
        ? Number((agentRes.cachedTokens / agentRes.inputTokens).toFixed(4))
        : null;

    return {
      schema: "bubblepaw.agentic-bench.v1",
      timestamp: new Date().toISOString(),
      task: task.meta.id,
      task_category: task.meta.category,
      agent: adapter.name,
      agent_version: agentVersion,
      model,
      rep,
      passed: checkRes.passed,
      check_details: checkRes.details || null,
      wall_ms: agentRes.wallMs,
      first_text_ms: agentRes.firstTextMs,
      requests: agentRes.requests,
      input_tokens: agentRes.inputTokens,
      output_tokens: agentRes.outputTokens,
      cached_tokens: agentRes.cachedTokens,
      cache_creation_tokens: agentRes.cacheCreationTokens,
      reasoning_tokens: agentRes.reasoningTokens,
      cost_usd: agentRes.costUsd,
      cost_source: agentRes.costSource,
      tool_calls: agentRes.toolCalls,
      tool_errors: agentRes.toolErrors,
      exit_code: agentRes.exitCode,
      error: agentRes.error,
      timed_out: agentRes.timedOut,
      files_changed: filesChanged,
      cache_hit_ratio: cacheHitRatio,
      unexplained_cache_misses: agentRes.unexplainedCacheMisses,
      repeated_tool_calls: agentRes.repeatedToolCalls,
      answer_chars: agentRes.finalText.length,
    };
  } finally {
    await rm(tempRoot, { recursive: true, force: true });
  }
}

export async function runBenchmark(options: RunnerOptions): Promise<BenchmarkResult> {
  if (options.agents.length === 0) {
    throw new Error("At least one agent must be specified");
  }
  const tasksDir = options.tasksDir ?? defaultTasksDir();
  const tasks = await discoverTasks(tasksDir, options.tasks);
  if (tasks.length === 0) {
    throw new Error(`No tasks found in ${tasksDir}`);
  }

  const reps = options.reps ?? 1;
  if (!Number.isInteger(reps) || reps < 1) {
    throw new Error(`Invalid reps count: ${reps}`);
  }

  const model = options.model ?? DEFAULT_MODEL;
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const interleave = options.interleave ?? true;

  const adapters = new Map<string, AgentAdapter>();
  const versions = new Map<string, string | null>();
  for (const agentName of options.agents) {
    if (!adapters.has(agentName)) {
      const adapter = createAdapter(agentName, options.adapterConfigs?.[agentName]);
      adapters.set(agentName, adapter);
      versions.set(agentName, await adapter.version());
    }
  }

  await mkdir(dirname(options.outFile), { recursive: true });
  await writeFile(options.outFile, "", "utf8");

  const schedule = buildSchedule(tasks, options.agents, reps, interleave, options.seedOrder);
  const records: RunRecord[] = [];

  for (const item of schedule) {
    const adapter = adapters.get(item.agentName)!;
    const agentVersion = versions.get(item.agentName) ?? null;
    const record = await runSingleExecution({
      task: item.task,
      adapter,
      agentVersion,
      model,
      rep: item.rep,
      timeoutMs,
      maxRounds: options.maxRounds,
      keepWorkspacesDir: options.keepWorkspacesDir,
    });
    records.push(record);
    await appendFile(options.outFile, `${JSON.stringify(record)}\n`, "utf8");
    options.onRecord?.(record);
  }

  return { outFile: options.outFile, records };
}
