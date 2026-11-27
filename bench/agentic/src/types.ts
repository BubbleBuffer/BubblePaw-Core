export type TaskCategory =
  | "find-fix"
  | "write-against-tests"
  | "multi-file-refactor"
  | "read-answer"
  | "shell-use"
  | "constraint-respect"
  | "harness-trap"
  | "long-horizon";

export interface RunRecord {
  schema: "bubblepaw.agentic-bench.v1";
  timestamp: string;
  task: string;
  task_category: string;
  agent: string;
  agent_version: string | null;
  model: string;
  rep: number;
  passed: boolean;
  check_details: string | null;
  wall_ms: number;
  first_text_ms: number | null;
  requests: number | null;
  input_tokens: number | null;
  output_tokens: number | null;
  cached_tokens: number | null;
  cache_creation_tokens: number | null;
  reasoning_tokens: number | null;
  cost_usd: number | null;
  cost_source: "agent" | "catalog" | null;
  tool_calls: number | null;
  tool_errors: number | null;
  exit_code: number | null;
  error: string | null;
  timed_out: boolean;
  files_changed: number;
  cache_hit_ratio: number | null;
  unexplained_cache_misses: number | null;
  repeated_tool_calls: number | null;
  answer_chars: number | null;
}

export interface TaskMeta {
  id: string;
  title: string;
  category: TaskCategory | string;
  expected_steps: string;
  timeout_ms?: number;
  description?: string;
}

export interface TaskDefinition {
  meta: TaskMeta;
  dir: string;
  promptPath: string;
  prompt: string;
  seedDir: string;
  checkPath: string;
}

export interface CheckContext {
  workspace: string;
  seedDir: string;
  finalText: string;
  taskId: string;
}

export interface CheckResult {
  passed: boolean;
  details: string;
}

export interface AgentRunRequest {
  taskId: string;
  prompt: string;
  promptFile: string;
  workspace: string;
  scratchDir: string;
  model: string;
  timeoutMs: number;
  maxRounds?: number;
  artifactDir?: string;
}

export interface AgentRunResult {
  finalText: string;
  wallMs: number;
  firstTextMs: number | null;
  requests: number | null;
  inputTokens: number | null;
  outputTokens: number | null;
  cachedTokens: number | null;
  cacheCreationTokens: number | null;
  reasoningTokens: number | null;
  costUsd: number | null;
  costSource: "agent" | "catalog" | null;
  toolCalls: number | null;
  toolErrors: number | null;
  exitCode: number | null;
  error: string | null;
  timedOut: boolean;
  unexplainedCacheMisses: number | null;
  repeatedToolCalls: number | null;
}

export interface AdapterConfig {
  bin?: string;
  bubblepawConfig?: string;
  piProvider?: string;
  piModel?: string;
  piTools?: string;
  piExtraArgs?: string[];
  maxRounds?: number;
}

export interface AgentAdapter {
  readonly name: string;
  version(): Promise<string | null>;
  run(request: AgentRunRequest): Promise<AgentRunResult>;
}

export interface RunnerOptions {
  tasksDir?: string;
  tasks?: string[];
  agents: string[];
  model?: string;
  reps?: number;
  timeoutMs?: number;
  maxRounds?: number;
  outFile: string;
  interleave?: boolean;
  seedOrder?: number;
  keepWorkspacesDir?: string;
  adapterConfigs?: Record<string, AdapterConfig>;
  onRecord?: (record: RunRecord) => void;
}

export interface BenchmarkResult {
  outFile: string;
  records: RunRecord[];
}
