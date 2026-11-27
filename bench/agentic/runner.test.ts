import { describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { formatMarkdownReport, loadRecords } from "./src/report.ts";
import { runBenchmark } from "./src/runner.ts";
import type { RunRecord } from "./src/types.ts";

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
