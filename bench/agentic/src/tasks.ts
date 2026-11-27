import { cp, readdir, readFile, stat, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { snapshotHashes } from "./check-helpers.ts";
import type { CheckResult, TaskDefinition, TaskMeta } from "./types.ts";

export function defaultTasksDir(): string {
  const here = dirname(fileURLToPath(import.meta.url));
  return resolve(here, "../tasks");
}

async function existsDir(path: string): Promise<boolean> {
  try {
    return (await stat(path)).isDirectory();
  } catch {
    return false;
  }
}

async function existsFile(path: string): Promise<boolean> {
  try {
    return (await stat(path)).isFile();
  } catch {
    return false;
  }
}

export async function discoverTasks(
  tasksDir: string = defaultTasksDir(),
  filterIds?: string[],
): Promise<TaskDefinition[]> {
  const entries = await readdir(tasksDir, { withFileTypes: true });
  const taskDirs = entries
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort();

  const tasksById = new Map<string, TaskDefinition>();
  for (const dirName of taskDirs) {
    const dir = join(tasksDir, dirName);
    const metaPath = join(dir, "task.json");
    const promptPath = join(dir, "prompt.md");
    const seedDir = join(dir, "seed");
    const checkPath = join(dir, "check.ts");

    if (!(await existsFile(metaPath))) {
      continue;
    }
    if (!(await existsFile(promptPath))) {
      throw new Error(`Task ${dirName} is missing prompt.md`);
    }
    if (!(await existsDir(seedDir))) {
      throw new Error(`Task ${dirName} is missing seed/ directory`);
    }
    if (!(await existsFile(checkPath))) {
      throw new Error(`Task ${dirName} is missing check.ts`);
    }

    const meta = JSON.parse(await readFile(metaPath, "utf8")) as TaskMeta;
    const prompt = await readFile(promptPath, "utf8");
    const id = meta.id || dirName;
    tasksById.set(id, {
      meta: { ...meta, id },
      dir,
      promptPath,
      prompt,
      seedDir,
      checkPath,
    });
  }

  if (!filterIds || filterIds.length === 0) {
    return [...tasksById.values()].sort((a, b) => a.meta.id.localeCompare(b.meta.id));
  }

  const selected: TaskDefinition[] = [];
  for (const id of filterIds) {
    const found = tasksById.get(id);
    if (!found) {
      const available = [...tasksById.keys()].sort().join(", ");
      throw new Error(`Unknown task "${id}". Available tasks: ${available}`);
    }
    selected.push(found);
  }
  return selected;
}

export async function prepareWorkspace(
  seedDir: string,
  workspaceDir: string,
): Promise<Map<string, string>> {
  // Only the seed directory enters the agent's temporary workspace; check.ts is never copied.
  await cp(seedDir, workspaceDir, { recursive: true, force: true });
  return snapshotHashes(workspaceDir);
}

export async function runTaskCheck(
  task: TaskDefinition,
  workspaceDir: string,
  finalText: string,
  scratchDir: string,
  timeoutMs = 30_000,
): Promise<CheckResult> {
  const here = dirname(fileURLToPath(import.meta.url));
  const checkRunnerPath = resolve(here, "check-runner.ts");
  const finalTextPath = join(scratchDir, "final-text.txt");
  const resultOutPath = join(scratchDir, "check-result.json");
  await writeFile(finalTextPath, finalText, "utf8");

  const proc = Bun.spawn(
    [
      process.execPath,
      "run",
      checkRunnerPath,
      task.checkPath,
      workspaceDir,
      task.seedDir,
      finalTextPath,
      resultOutPath,
      task.meta.id,
    ],
    {
      cwd: workspaceDir,
      stdout: "pipe",
      stderr: "pipe",
      env: { ...process.env, NO_COLOR: "1" },
    },
  );

  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    try {
      proc.kill();
    } catch {
      // ignore kill race
    }
  }, timeoutMs);

  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  clearTimeout(timer);

  if (timedOut) {
    return {
      passed: false,
      details: `Check timed out after ${timeoutMs}ms`,
    };
  }

  try {
    const raw = await readFile(resultOutPath, "utf8");
    const parsed = JSON.parse(raw) as CheckResult;
    return {
      passed: Boolean(parsed.passed),
      details: typeof parsed.details === "string" ? parsed.details : "",
    };
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    const out = (stdout + "\n" + stderr).trim();
    return {
      passed: false,
      details: `Check runner failed (exit ${exitCode}): ${msg}${out ? `\n${out.slice(0, 1000)}` : ""}`,
    };
  }
}
