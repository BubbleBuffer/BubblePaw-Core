import { createHash } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import { join, relative } from "node:path";
import { pathToFileURL } from "node:url";

export function toPosixRel(root: string, fullPath: string): string {
  return relative(root, fullPath).replaceAll("\\", "/");
}

export async function sha256File(filePath: string): Promise<string> {
  const data = await readFile(filePath);
  return createHash("sha256").update(data).digest("hex");
}

export async function listRelativeFiles(
  dir: string,
  ignoreDirs: Set<string> = new Set([".git", "node_modules", ".bubblepaw"]),
): Promise<string[]> {
  const out: string[] = [];
  async function walk(current: string): Promise<void> {
    const entries = await readdir(current, { withFileTypes: true });
    for (const entry of entries) {
      if (ignoreDirs.has(entry.name)) {
        continue;
      }
      const full = join(current, entry.name);
      if (entry.isDirectory()) {
        await walk(full);
      } else if (entry.isFile()) {
        out.push(toPosixRel(dir, full));
      }
    }
  }
  await walk(dir);
  out.sort();
  return out;
}

export async function snapshotHashes(
  dir: string,
  ignoreDirs?: Set<string>,
): Promise<Map<string, string>> {
  const files = await listRelativeFiles(dir, ignoreDirs);
  const map = new Map<string, string>();
  for (const rel of files) {
    map.set(rel, await sha256File(join(dir, rel)));
  }
  return map;
}

export async function countChangedFiles(
  before: Map<string, string>,
  workspaceDir: string,
): Promise<number> {
  const after = await snapshotHashes(workspaceDir);
  const keys = new Set<string>([...before.keys(), ...after.keys()]);
  let changed = 0;
  for (const key of keys) {
    if (before.get(key) !== after.get(key)) {
      changed += 1;
    }
  }
  return changed;
}

export async function assertFilesUnchanged(
  workspaceDir: string,
  seedDir: string,
  relPaths: string[],
): Promise<void> {
  for (const rel of relPaths) {
    const normalized = rel.replaceAll("\\", "/");
    const seedPath = join(seedDir, normalized);
    const workPath = join(workspaceDir, normalized);
    const seedHash = await sha256File(seedPath);
    let workHash: string;
    try {
      workHash = await sha256File(workPath);
    } catch {
      throw new Error(`Immutable file missing in workspace: ${normalized}`);
    }
    if (seedHash !== workHash) {
      throw new Error(`Immutable file was modified: ${normalized}`);
    }
  }
}

export async function assertExactFileSet(
  workspaceDir: string,
  seedDir: string,
  allowedExtra: string[] = [],
): Promise<void> {
  const expected = new Set<string>(await listRelativeFiles(seedDir));
  for (const extra of allowedExtra) {
    expected.add(extra.replaceAll("\\", "/"));
  }
  const actual = await listRelativeFiles(workspaceDir);
  const unexpected = actual.filter((f) => !expected.has(f));
  const missing = [...expected].filter((f) => !actual.includes(f));
  if (unexpected.length > 0) {
    throw new Error(`Unexpected extra files in workspace: ${unexpected.join(", ")}`);
  }
  if (missing.length > 0) {
    throw new Error(`Expected files missing from workspace: ${missing.join(", ")}`);
  }
}

export async function importWorkspaceModule<T = Record<string, unknown>>(
  workspaceDir: string,
  relPath: string,
): Promise<T> {
  const full = join(workspaceDir, relPath);
  const url = `${pathToFileURL(full).href}?v=${Date.now()}_${Math.random().toString(36).slice(2)}`;
  return (await import(url)) as T;
}

export interface BunRunOutput {
  exitCode: number;
  stdout: string;
  stderr: string;
}

export async function runBunCommand(
  args: string[],
  cwd: string,
  timeoutMs = 20_000,
): Promise<BunRunOutput> {
  const proc = Bun.spawn([process.execPath, ...args], {
    cwd,
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, NO_COLOR: "1" },
  });

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
    throw new Error(`Subprocess timed out after ${timeoutMs}ms: bun ${args.join(" ")}`);
  }
  return { exitCode, stdout, stderr };
}

export async function runBunTest(workspaceDir: string, extraArgs: string[] = []): Promise<void> {
  const res = await runBunCommand(["test", ...extraArgs], workspaceDir);
  if (res.exitCode !== 0) {
    throw new Error(
      `bun test failed (exit ${res.exitCode}):\n${(res.stdout + "\n" + res.stderr).trim().slice(0, 2000)}`,
    );
  }
}

export async function runBunScript(
  workspaceDir: string,
  relScriptPath: string,
  args: string[] = [],
): Promise<BunRunOutput> {
  return runBunCommand(["run", relScriptPath, ...args], workspaceDir);
}
