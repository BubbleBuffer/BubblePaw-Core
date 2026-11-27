import assert from "node:assert/strict";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { assertFilesUnchanged } from "../../src/check-helpers.ts";
import type { CheckContext, CheckResult } from "../../src/types.ts";

async function computeExpectedReport(seedDataDir: string) {
  const names = (await readdir(seedDataDir)).filter((f) => f.endsWith(".jsonl")).sort();
  let totalLines = 0;
  let malformedLines = 0;
  let duplicateEvents = 0;
  const seen = new Set<string>();
  const failedTraceSet = new Set<string>();
  const errorCounts: Record<string, number> = {};
  const byService = new Map<string, { durations: number[]; errorCount: number }>();

  for (const name of names) {
    const raw = await readFile(join(seedDataDir, name), "utf8");
    for (const rawLine of raw.split(/\r?\n/)) {
      if (rawLine.trim().length === 0) {
        continue;
      }
      totalLines += 1;
      let ev: Record<string, unknown>;
      try {
        ev = JSON.parse(rawLine) as Record<string, unknown>;
      } catch {
        malformedLines += 1;
        continue;
      }
      if (
        !ev ||
        typeof ev !== "object" ||
        typeof ev.event_id !== "string" ||
        typeof ev.service !== "string" ||
        typeof ev.status !== "number" ||
        typeof ev.duration_ms !== "number" ||
        typeof ev.trace_id !== "string"
      ) {
        malformedLines += 1;
        continue;
      }
      if (seen.has(ev.event_id)) {
        duplicateEvents += 1;
        continue;
      }
      seen.add(ev.event_id);

      let svc = byService.get(ev.service);
      if (!svc) {
        svc = { durations: [], errorCount: 0 };
        byService.set(ev.service, svc);
      }
      svc.durations.push(ev.duration_ms);
      if (ev.status >= 500) {
        svc.errorCount += 1;
        failedTraceSet.add(ev.trace_id);
        if (typeof ev.error_code === "string" && ev.error_code.length > 0) {
          errorCounts[ev.error_code] = (errorCounts[ev.error_code] ?? 0) + 1;
        }
      }
    }
  }

  const services: Record<string, { count: number; error_count: number; p95_duration_ms: number }> =
    {};
  for (const name of [...byService.keys()].sort()) {
    const svc = byService.get(name)!;
    const sorted = [...svc.durations].sort((a, b) => a - b);
    const idx = Math.ceil(0.95 * sorted.length) - 1;
    services[name] = {
      count: sorted.length,
      error_count: svc.errorCount,
      p95_duration_ms: sorted[idx]!,
    };
  }

  const sortedErrors: Record<string, number> = {};
  for (const key of Object.keys(errorCounts).sort()) {
    sortedErrors[key] = errorCounts[key]!;
  }

  return {
    total_lines: totalLines,
    malformed_lines: malformedLines,
    unique_events: seen.size,
    duplicate_events: duplicateEvents,
    failed_trace_ids: [...failedTraceSet].sort(),
    error_counts: sortedErrors,
    services,
  };
}

export async function check(ctx: CheckContext): Promise<CheckResult> {
  await assertFilesUnchanged(ctx.workspace, ctx.seedDir, [
    "data/app-01.jsonl",
    "data/app-02.jsonl",
    "data/app-03.jsonl",
    "data/app-04.jsonl",
  ]);

  const raw = await readFile(join(ctx.workspace, "report.json"), "utf8");
  const actual = JSON.parse(raw) as Record<string, unknown>;
  const expected = await computeExpectedReport(join(ctx.seedDir, "data"));

  assert.deepEqual(actual, expected, "report.json does not match exact log aggregation");
  return { passed: true, details: "report.json matches exact deduplicated log forensics." };
}
