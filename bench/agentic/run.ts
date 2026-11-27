import { writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { formatMarkdownReport, loadRecords } from "./src/report.ts";
import { DEFAULT_MODEL, DEFAULT_TIMEOUT_MS, runBenchmark } from "./src/runner.ts";
import { defaultTasksDir, discoverTasks } from "./src/tasks.ts";
import type { AdapterConfig } from "./src/types.ts";

function printHelp(): void {
  console.log(`Usage: bun run bench/agentic/run.ts [options]

Options:
  --agent <name>              Agent adapter to run (repeatable or comma-separated: bubblepaw, pi, fake, fake-noop)
  --task <id>                 Task ID to run (repeatable or comma-separated; default: all)
  --reps <n>                  Repetitions per (task, agent) (default: 1)
  --model <provider/model>    Model identifier (default: ${DEFAULT_MODEL})
  --bin <path>                Executable path for the single selected agent
  --bubblepaw-bin <path>      Executable path for bubblepaw
  --pi-bin <path>             Executable path for pi
  --bubblepaw-config <path>   Custom TOML config for bubblepaw
  --pi-provider <name>        Provider flag passed to pi (default inferred from --model)
  --pi-model <id>             Model flag passed to pi (default inferred from --model)
  --pi-tools <list>           Tools list passed to pi --tools
  --pi-node <path>            Node executable that runs --pi-bin (a JS entry); env PI_NODE
  --pi-thinking <level>       pi --thinking level (default low, matching bubblepaw.toml)
  --pi-extra-args <csv>       Comma-separated extra args passed to pi
  --timeout-ms <ms>           Per-task timeout in milliseconds (default: ${DEFAULT_TIMEOUT_MS})
  --max-rounds <n>            Override agent.max_rounds for bubblepaw
  --out <path>                JSONL output file (default: bench/agentic/results/<timestamp>.jsonl)
  --baseline <path>           Optional baseline JSONL file to compare against in the report
  --no-interleave             Run each agent sequentially instead of alternating per task/rep
  --seed <n>                  Deterministic shuffle seed within interleaved reps
  --keep-workspaces <dir>     Preserve final workspaces and raw logs under <dir>
  --list                      List available tasks and exit
  --help                      Show this help message
`);
}

function splitCsvArg(val: string): string[] {
  return val
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

async function main(): Promise<void> {
  const argv = Bun.argv.slice(2);
  const agents: string[] = [];
  const tasks: string[] = [];
  let reps = 1;
  let model = DEFAULT_MODEL;
  let sharedBin: string | undefined;
  let bubblepawBin: string | undefined;
  let piBin: string | undefined;
  let bubblepawConfig: string | undefined;
  let piProvider: string | undefined;
  let piModel: string | undefined;
  let piTools: string | undefined;
  let piNode: string | undefined;
  let piThinking: string | undefined;
  let piExtraArgs: string[] | undefined;
  let timeoutMs = DEFAULT_TIMEOUT_MS;
  let maxRounds: number | undefined;
  let outFile: string | undefined;
  let baselineFile: string | undefined;
  let interleave = true;
  let seedOrder: number | undefined;
  let keepWorkspacesDir: string | undefined;
  let listOnly = false;

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i]!;
    const next = (): string => {
      const v = argv[i + 1];
      if (!v) throw new Error(`Missing value for ${arg}`);
      i += 1;
      return v;
    };

    switch (arg) {
      case "--help":
      case "-h":
        printHelp();
        return;
      case "--list":
        listOnly = true;
        break;
      case "--agent":
        agents.push(...splitCsvArg(next()));
        break;
      case "--task":
        tasks.push(...splitCsvArg(next()));
        break;
      case "--reps":
        reps = Number(next());
        break;
      case "--model":
        model = next();
        break;
      case "--bin":
        sharedBin = next();
        break;
      case "--bubblepaw-bin":
        bubblepawBin = next();
        break;
      case "--pi-bin":
        piBin = next();
        break;
      case "--bubblepaw-config":
        bubblepawConfig = resolve(next());
        break;
      case "--pi-provider":
        piProvider = next();
        break;
      case "--pi-model":
        piModel = next();
        break;
      case "--pi-tools":
        piTools = next();
        break;
      case "--pi-node":
        piNode = next();
        break;
      case "--pi-thinking":
        piThinking = next();
        break;
      case "--pi-extra-args":
        piExtraArgs = splitCsvArg(next());
        break;
      case "--timeout-ms":
        timeoutMs = Number(next());
        break;
      case "--max-rounds":
        maxRounds = Number(next());
        break;
      case "--out":
        outFile = resolve(next());
        break;
      case "--baseline":
        baselineFile = resolve(next());
        break;
      case "--no-interleave":
        interleave = false;
        break;
      case "--seed":
        seedOrder = Number(next());
        break;
      case "--keep-workspaces":
        keepWorkspacesDir = resolve(next());
        break;
      default:
        throw new Error(`Unknown option: ${arg}`);
    }
  }

  if (listOnly) {
    const discovered = await discoverTasks(defaultTasksDir());
    for (const t of discovered) {
      console.log(`${t.meta.id}\t[${t.meta.category}]\t(${t.meta.expected_steps} steps)\t${t.meta.title}`);
    }
    return;
  }

  const selectedAgents = agents.length > 0 ? agents : ["bubblepaw"];
  const here = dirname(fileURLToPath(import.meta.url));
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const resolvedOut = outFile ?? join(here, "results", `run-${stamp}.jsonl`);

  const adapterConfigs: Record<string, AdapterConfig> = {
    bubblepaw: {
      bin: bubblepawBin ?? (selectedAgents.length === 1 ? sharedBin : undefined),
      bubblepawConfig,
      maxRounds,
    },
    pi: {
      bin: piBin ?? (selectedAgents.length === 1 ? sharedBin : undefined),
      piProvider,
      piModel,
      piTools,
      piNode,
      piThinking,
      piExtraArgs,
    },
  };

  const result = await runBenchmark({
    tasks: tasks.length > 0 ? tasks : undefined,
    agents: selectedAgents,
    model,
    reps,
    timeoutMs,
    maxRounds,
    outFile: resolvedOut,
    interleave,
    seedOrder,
    keepWorkspacesDir,
    adapterConfigs,
    onRecord: (rec) => {
      const status = rec.passed ? "PASS" : "FAIL";
      const cost = rec.cost_usd !== null ? `$${rec.cost_usd.toFixed(4)}` : "$—";
      console.error(
        `[${status}] ${rec.task} (${rec.agent} rep ${rec.rep}) ${rec.wall_ms}ms ${cost}${rec.error ? ` err=${rec.error}` : ""}`,
      );
    },
  });

  const baselineRecords = baselineFile ? await loadRecords(baselineFile) : undefined;
  const markdown = formatMarkdownReport(result.records, { baselineRecords });
  console.log(markdown);

  if (outFile) {
    const summaryPath = outFile.replace(/\.jsonl$/i, ".md");
    if (summaryPath !== outFile) {
      await writeFile(summaryPath, markdown, "utf8");
    }
  }
}

await main();
