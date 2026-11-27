import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { bubblepawRunArgs } from "./src/adapters/bubblepaw.ts";
import { PiAdapter } from "./src/adapters/pi.ts";
import { snapshotHashes } from "./src/check-helpers.ts";
import { formatMarkdownReport, loadRecords } from "./src/report.ts";
import { runBenchmark } from "./src/runner.ts";
import { defaultTasksDir, discoverTasks, prepareWorkspace } from "./src/tasks.ts";
import { check } from "./tasks/09-long-haul/check.ts";
import { CONSTRAINTS, writeReference } from "./tasks/09-long-haul/reference.ts";
import type { RunRecord } from "./src/types.ts";

const LONG = "09-long-haul";

describe("long-context task 09-long-haul", () => {
  test("is opt-in: left out of the default set, run when named", async () => {
    const dir = await mkdtemp(join(tmpdir(), "bp-long-optin-"));
    try {
      const defaults = await discoverTasks(defaultTasksDir());
      expect(defaults.map((t) => t.meta.id)).not.toContain(LONG);
      expect(defaults).toHaveLength(8);
      const named = await runBenchmark({ tasks: [LONG], agents: ["fake"], outFile: join(dir, "r.jsonl") });
      expect(named.records.map((r) => r.task)).toEqual([LONG]);
      expect(named.records[0]!.passed, named.records[0]!.check_details ?? "").toBe(true);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  }, 60_000);

  test("setup hook writes the same notes every time, about a million tokens of them", async () => {
    const [task] = await discoverTasks(defaultTasksDir(), [LONG]);
    const dirs = [await mkdtemp(join(tmpdir(), "bp-long-a-")), await mkdtemp(join(tmpdir(), "bp-long-b-"))];
    try {
      const [a, b] = await Promise.all(dirs.map((d) => prepareWorkspace(task!.seedDir, d, task!.setupPath)));
      expect([...a!.entries()]).toEqual([...b!.entries()]);
      expect([...a!.keys()].filter((k) => k.startsWith("material/stage-")).length).toBeGreaterThanOrEqual(160);
      // Volume guard: 3 MB of notes is about 0.8M tokens read, about 1M with the agent's own work.
      let bytes = 0;
      for (const rel of a!.keys()) bytes += (await stat(join(dirs[0]!, rel))).size;
      expect(bytes).toBeGreaterThan(3_000_000);
      expect(bytes).toBeLessThan(3_800_000);
      // The rules and the truth are never in the workspace.
      expect([...a!.keys()].some((k) => /check|reference|world|prompt/.test(k))).toBe(false);
    } finally {
      await Promise.all(dirs.map((d) => rm(d, { recursive: true, force: true })));
    }
  }, 60_000);

  test("check passes the reference and names the stage and constraint a break starts at", async () => {
    const dir = await mkdtemp(join(tmpdir(), "bp-long-check-"));
    const ctx = (workspace: string) => ({ workspace, seedDir: dir, finalText: "", taskId: LONG });
    try {
      await writeReference(dir);
      const ok = await check(ctx(dir));
      expect(ok.passed).toBe(true);

      const fromStage = 9;
      for (const constraint of CONSTRAINTS) {
        const ws = join(dir, constraint);
        await mkdir(ws, { recursive: true });
        await writeReference(ws, { constraint, fromStage });
        const res = await check(ctx(ws));
        const d = JSON.parse(res.details) as {
          stages_correct: number[];
          constraints_kept_by_stage: Record<string, string>;
          first_lost: Record<string, number | null>;
        };
        expect(res.passed, constraint).toBe(false);
        expect(d.stages_correct, constraint).toEqual(Array.from({ length: fromStage - 1 }, (_, i) => i + 1));
        for (const c of CONSTRAINTS) {
          if (c === constraint) {
            expect(d.constraints_kept_by_stage[c], c).toBe("1".repeat(fromStage - 1) + "0".repeat(20 - fromStage + 1));
            expect(d.first_lost[c], c).toBe(fromStage);
          } else {
            expect(d.constraints_kept_by_stage[c], `${c} while breaking ${constraint}`).toBe("1".repeat(20));
          }
        }
      }

      const noop = await runBenchmark({ tasks: [LONG], agents: ["fake-noop"], outFile: join(dir, "n.jsonl") });
      expect(noop.records[0]!.passed).toBe(false);
      expect(JSON.parse(noop.records[0]!.check_details!).stages_missing).toHaveLength(20);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  }, 120_000);

  test("--context-window reaches BubblePaw's command line, pi's agent directory and the record", async () => {
    const req = {
      taskId: LONG, prompt: "p", promptFile: "p.md", workspace: "w", scratchDir: "s", model: "m", timeoutMs: 1000,
    };
    const args = bubblepawRunArgs("bp", "c.toml", "d", "t", { ...req, contextWindow: 100_000 }, undefined);
    expect(args.join(" ")).toContain("--set context.window_tokens=100000 --set context.compact_threshold_tokens=100000");
    expect(bubblepawRunArgs("bp", "c.toml", "d", "t", req, undefined).join(" ")).not.toContain("context.");

    // A stub "pi" (a JS file run by bun) reports the agent directory it was started with.
    const dir = await mkdtemp(join(tmpdir(), "bp-long-pi-"));
    try {
      const stub = join(dir, "stub.js");
      await writeFile(
        stub,
        `const fs = require("node:fs");
         const d = process.env.PI_CODING_AGENT_DIR;
         if (d) fs.writeFileSync(process.env.STUB_OUT, JSON.stringify({ d, models: fs.readFileSync(d + "/models.json", "utf8"), settings: fs.readFileSync(d + "/settings.json", "utf8") }));`,
      );
      const seen = join(dir, "seen.json");
      process.env.STUB_OUT = seen;
      const scratch = join(dir, "scratch");
      await mkdir(scratch, { recursive: true });
      const pi = new PiAdapter({ bin: stub, piNode: process.execPath });
      await pi.run({ ...req, workspace: dir, scratchDir: scratch, model: "opencode/deepseek-v4.1-flash", contextWindow: 250_000 });
      const got = JSON.parse(await readFile(seen, "utf8")) as { d: string; models: string; settings: string };
      expect(got.d).toBe(join(scratch, "pi-agent"));
      expect(JSON.parse(got.models).providers["opencode-go"].modelOverrides["deepseek-v4.1-flash"]).toEqual({ contextWindow: 250000 });
      expect(JSON.parse(got.settings).compaction.enabled).toBe(true);
      await rm(seen);
      await pi.run({ ...req, workspace: dir, scratchDir: scratch, model: "opencode/deepseek-v4.1-flash" });
      await expect(stat(seen)).rejects.toThrow();

      const rec = await runBenchmark({
        tasks: ["01-one-line-bug"], agents: ["fake"], contextWindow: 100_000, outFile: join(dir, "r.jsonl"),
      });
      expect(rec.records[0]!.context_window).toBe(100000);
    } finally {
      delete process.env.STUB_OUT;
      await rm(dir, { recursive: true, force: true });
    }
  }, 60_000);
});

describe("agentic benchmark runner", () => {
  test("fake agent passes 01-one-line-bug, writes JSONL record, and hides check.ts from workspace", async () => {
    const dir = await mkdtemp(join(tmpdir(), "bp-runner-test-"));
    try {
      const outFile = join(dir, "results.jsonl");
      const keptDir = join(dir, "kept");
      const res = await runBenchmark({
        tasks: ["01-one-line-bug"],
        agents: ["fake"],
        model: "opencode/deepseek-v4.1-flash",
        reps: 1,
        outFile,
        keepWorkspacesDir: keptDir,
      });

      expect(res.records).toHaveLength(1);
      const rec = res.records[0]!;
      expect(rec.schema).toBe("bubblepaw.agentic-bench.v1");
      expect(rec.task).toBe("01-one-line-bug");
      expect(rec.agent).toBe("fake");
      expect(rec.agent_version).toBe("fake-1.0.0");
      expect(rec.model).toBe("opencode/deepseek-v4.1-flash");
      expect(rec.rep).toBe(1);
      expect(rec.passed).toBe(true);
      expect(rec.wall_ms).toBeGreaterThanOrEqual(0);
      expect(rec.exit_code).toBe(0);
      expect(rec.error).toBeNull();
      expect(rec.files_changed).toBe(1);

      const rawJsonl = await readFile(outFile, "utf8");
      const parsed = JSON.parse(rawJsonl.trim()) as RunRecord;
      expect(parsed.passed).toBe(true);
      expect(parsed.task).toBe("01-one-line-bug");

      // Confirm check.ts was never copied into the agent's workspace.
      const leakedCheck = join(keptDir, "01-one-line-bug__fake__rep1", "workspace", "check.ts");
      await expect(stat(leakedCheck)).rejects.toThrow();
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("fake-noop agent fails 01-one-line-bug", async () => {
    const dir = await mkdtemp(join(tmpdir(), "bp-runner-noop-"));
    try {
      const outFile = join(dir, "noop.jsonl");
      const res = await runBenchmark({
        tasks: ["01-one-line-bug"],
        agents: ["fake-noop"],
        outFile,
      });

      expect(res.records).toHaveLength(1);
      expect(res.records[0]!.passed).toBe(false);
      expect(res.records[0]!.check_details).toBeTruthy();
      expect(res.records[0]!.files_changed).toBe(0);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("all eight tasks pass with fake and fail with fake-noop", async () => {
    const dir = await mkdtemp(join(tmpdir(), "bp-runner-all-"));
    try {
      const solveOut = join(dir, "solve.jsonl");
      const noopOut = join(dir, "noop.jsonl");

      const solveRes = await runBenchmark({
        agents: ["fake"],
        outFile: solveOut,
      });
      expect(solveRes.records).toHaveLength(8);
      for (const rec of solveRes.records) {
        expect(rec.passed, `expected fake to pass ${rec.task}: ${rec.check_details}`).toBe(true);
      }

      const noopRes = await runBenchmark({
        agents: ["fake-noop"],
        outFile: noopOut,
      });
      expect(noopRes.records).toHaveLength(8);
      for (const rec of noopRes.records) {
        expect(rec.passed, `expected fake-noop to fail ${rec.task}`).toBe(false);
      }

      const loadedSolve = await loadRecords(solveOut);
      const loadedNoop = await loadRecords(noopOut);
      const report = formatMarkdownReport(loadedSolve, {
        baselineRecords: loadedNoop,
        candidateLabel: "fake",
        baselineLabel: "fake-noop",
      });
      expect(report).toContain("## Overall Summary");
      expect(report).toContain("## Per-Task Comparison");
      expect(report).toContain("8/8 (100.0%)");
      expect(report).toContain("0/8 (0.0%)");
      expect(report).toContain("08-multi-step-migration");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});
