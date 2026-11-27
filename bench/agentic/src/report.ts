import { readFile } from "node:fs/promises";
import type { RunRecord } from "./types.ts";

export interface AggregateStats {
  runs: number;
  passed: number;
  passRate: number;
  allPassed: boolean;
  medianWallMs: number | null;
  medianFirstTextMs: number | null;
  medianRequests: number | null;
  medianInputTokens: number | null;
  medianCachedTokens: number | null;
  medianOutputTokens: number | null;
  medianCacheHitRatio: number | null;
  medianToolCalls: number | null;
  totalToolErrors: number;
  medianCostUsd: number | null;
  totalCostUsd: number | null;
  costPerSolvedUsd: number | null;
  unexplainedCacheMisses: number | null;
  repeatedToolCalls: number | null;
}

export async function loadRecords(filePath: string): Promise<RunRecord[]> {
  const raw = await readFile(filePath, "utf8");
  const records: RunRecord[] = [];
  for (const line of raw.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    records.push(JSON.parse(trimmed) as RunRecord);
  }
  return records;
}

export function median(values: number[]): number | null {
  if (values.length === 0) {
    return null;
  }
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  if (sorted.length % 2 === 1) {
    return sorted[mid]!;
  }
  return (sorted[mid - 1]! + sorted[mid]!) / 2;
}

function nums(records: RunRecord[], pick: (r: RunRecord) => number | null): number[] {
  const out: number[] = [];
  for (const r of records) {
    const v = pick(r);
    if (typeof v === "number" && Number.isFinite(v)) {
      out.push(v);
    }
  }
  return out;
}

function sumNullable(records: RunRecord[], pick: (r: RunRecord) => number | null): number | null {
  const values = nums(records, pick);
  if (values.length === 0) {
    return null;
  }
  return values.reduce((acc, v) => acc + v, 0);
}

export function aggregateRecords(records: RunRecord[]): AggregateStats {
  const runs = records.length;
  const passed = records.filter((r) => r.passed).length;
  const passRate = runs > 0 ? passed / runs : 0;
  const allPassed = runs > 0 && passed === runs;

  const costValues = nums(records, (r) => r.cost_usd);
  const totalCostUsd =
    records.length > 0 && costValues.length === records.length
      ? costValues.reduce((a, b) => a + b, 0)
      : costValues.length > 0
        ? costValues.reduce((a, b) => a + b, 0)
        : null;

  const costPerSolvedUsd =
    totalCostUsd !== null && passed > 0 ? totalCostUsd / passed : null;

  return {
    runs,
    passed,
    passRate,
    allPassed,
    medianWallMs: median(nums(records, (r) => r.wall_ms)),
    medianFirstTextMs: median(nums(records, (r) => r.first_text_ms)),
    medianRequests: median(nums(records, (r) => r.requests)),
    medianInputTokens: median(nums(records, (r) => r.input_tokens)),
    medianCachedTokens: median(nums(records, (r) => r.cached_tokens)),
    medianOutputTokens: median(nums(records, (r) => r.output_tokens)),
    medianCacheHitRatio: median(nums(records, (r) => r.cache_hit_ratio)),
    medianToolCalls: median(nums(records, (r) => r.tool_calls)),
    totalToolErrors: nums(records, (r) => r.tool_errors).reduce((a, b) => a + b, 0),
    medianCostUsd: median(costValues),
    totalCostUsd,
    costPerSolvedUsd,
    unexplainedCacheMisses: sumNullable(records, (r) => r.unexplained_cache_misses),
    repeatedToolCalls: sumNullable(records, (r) => r.repeated_tool_calls),
  };
}

function fmtPct(ratio: number | null): string {
  if (ratio === null) return "—";
  return `${(ratio * 100).toFixed(1)}%`;
}

function fmtMs(ms: number | null): string {
  if (ms === null) return "—";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

function fmtInt(n: number | null): string {
  if (n === null) return "—";
  return Math.round(n).toLocaleString("en-US");
}

function fmtCost(usd: number | null): string {
  if (usd === null) return "—";
  return `$${usd.toFixed(4)}`;
}

function fmtDeltaPctPoints(cand: number, base: number): string {
  const diff = (cand - base) * 100;
  const sign = diff > 0 ? "+" : "";
  return `${sign}${diff.toFixed(1)}pp`;
}

function fmtDeltaRatio(cand: number | null, base: number | null): string {
  if (cand === null || base === null || base === 0) return "—";
  const pct = ((cand - base) / base) * 100;
  const sign = pct > 0 ? "+" : "";
  return `${sign}${pct.toFixed(1)}%`;
}

function groupBy<T>(items: T[], keyFn: (item: T) => string): Map<string, T[]> {
  const map = new Map<string, T[]>();
  for (const item of items) {
    const key = keyFn(item);
    const bucket = map.get(key);
    if (bucket) {
      bucket.push(item);
    } else {
      map.set(key, [item]);
    }
  }
  return map;
}

function computePairedWlt(
  candRecords: RunRecord[],
  baseRecords: RunRecord[],
): { wins: number; losses: number; ties: number; pairedTasks: number } {
  const candTasks = groupBy(candRecords, (r) => r.task);
  const baseTasks = groupBy(baseRecords, (r) => r.task);
  let wins = 0;
  let losses = 0;
  let ties = 0;
  let pairedTasks = 0;
  for (const [task, cRuns] of candTasks.entries()) {
    const bRuns = baseTasks.get(task);
    if (!bRuns) continue;
    pairedTasks += 1;
    const cRate = cRuns.filter((r) => r.passed).length / cRuns.length;
    const bRate = bRuns.filter((r) => r.passed).length / bRuns.length;
    if (cRate > bRate) wins += 1;
    else if (cRate < bRate) losses += 1;
    else ties += 1;
  }
  return { wins, losses, ties, pairedTasks };
}

export interface ReportOptions {
  baselineRecords?: RunRecord[];
  candidateLabel?: string;
  baselineLabel?: string;
}

export function formatMarkdownReport(
  candidateRecords: RunRecord[],
  options: ReportOptions = {},
): string {
  const lines: string[] = [];
  const agents = [...new Set(candidateRecords.map((r) => r.agent))];

  // If no separate baseline is provided and candidate file has two agents (e.g. bubblepaw + pi),
  // render a side-by-side summary for each agent plus a comparison when baseline is given.
  if (options.baselineRecords && options.baselineRecords.length > 0) {
    const baseRecords = options.baselineRecords;
    const candLabel =
      options.candidateLabel ??
      (agents.length === 1 ? agents[0]! : `candidate (${agents.join(", ")})`);
    const baseAgents = [...new Set(baseRecords.map((r) => r.agent))];
    const baseLabel =
      options.baselineLabel ??
      (baseAgents.length === 1 ? baseAgents[0]! : `baseline (${baseAgents.join(", ")})`);

    const candModels = [...new Set(candidateRecords.map((r) => r.model))];
    const baseModels = [...new Set(baseRecords.map((r) => r.model))];
    if (candModels.join(",") !== baseModels.join(",")) {
      lines.push(
        `> Warning: model mismatch between ${candLabel} (${candModels.join(", ")}) and ${baseLabel} (${baseModels.join(", ")}).`,
      );
      lines.push("");
    }

    const candTotal = aggregateRecords(candidateRecords);
    const baseTotal = aggregateRecords(baseRecords);
    const wlt = computePairedWlt(candidateRecords, baseRecords);

    lines.push("## Overall Summary");
    lines.push("");
    lines.push(
      "| Arm | Runs | Pass Rate | Median Wall | Median In / Cached / Out | Cache Hit | Total Cost | Cost / Solved |",
    );
    lines.push(
      "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    );
    lines.push(
      `| ${candLabel} | ${candTotal.runs} | ${candTotal.passed}/${candTotal.runs} (${fmtPct(candTotal.passRate)}) | ${fmtMs(candTotal.medianWallMs)} | ${fmtInt(candTotal.medianInputTokens)} / ${fmtInt(candTotal.medianCachedTokens)} / ${fmtInt(candTotal.medianOutputTokens)} | ${fmtPct(candTotal.medianCacheHitRatio)} | ${fmtCost(candTotal.totalCostUsd)} | ${fmtCost(candTotal.costPerSolvedUsd)} |`,
    );
    lines.push(
      `| ${baseLabel} | ${baseTotal.runs} | ${baseTotal.passed}/${baseTotal.runs} (${fmtPct(baseTotal.passRate)}) | ${fmtMs(baseTotal.medianWallMs)} | ${fmtInt(baseTotal.medianInputTokens)} / ${fmtInt(baseTotal.medianCachedTokens)} / ${fmtInt(baseTotal.medianOutputTokens)} | ${fmtPct(baseTotal.medianCacheHitRatio)} | ${fmtCost(baseTotal.totalCostUsd)} | ${fmtCost(baseTotal.costPerSolvedUsd)} |`,
    );
    lines.push("");
    lines.push(
      `Paired tasks (${wlt.pairedTasks}): **${wlt.wins}W / ${wlt.losses}L / ${wlt.ties}T** | Pass Δ: **${fmtDeltaPctPoints(candTotal.passRate, baseTotal.passRate)}** | Wall Δ: **${fmtDeltaRatio(candTotal.medianWallMs, baseTotal.medianWallMs)}** | Cost Δ: **${fmtDeltaRatio(candTotal.totalCostUsd, baseTotal.totalCostUsd)}**`,
    );
    lines.push("");

    const candByTask = groupBy(candidateRecords, (r) => r.task);
    const baseByTask = groupBy(baseRecords, (r) => r.task);
    const allTasks = [...new Set([...candByTask.keys(), ...baseByTask.keys()])].sort();

    lines.push("## Per-Task Comparison");
    lines.push("");
    lines.push(
      "| Task | Pass | Base Pass | Δ Pass | Med Wall | Δ Wall | In / Out (Med) | Cache % | Cost | Δ Cost |",
    );
    lines.push(
      "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    );
    for (const taskId of allTasks) {
      const cRuns = candByTask.get(taskId) ?? [];
      const bRuns = baseByTask.get(taskId) ?? [];
      const c = aggregateRecords(cRuns);
      const b = aggregateRecords(bRuns);
      const cPassStr = c.runs > 0 ? `${c.passed}/${c.runs}` : "—";
      const bPassStr = b.runs > 0 ? `${b.passed}/${b.runs}` : "—";
      const deltaPass =
        c.runs > 0 && b.runs > 0 ? fmtDeltaPctPoints(c.passRate, b.passRate) : "—";
      lines.push(
        `| ${taskId} | ${cPassStr} | ${bPassStr} | ${deltaPass} | ${fmtMs(c.medianWallMs)} | ${fmtDeltaRatio(c.medianWallMs, b.medianWallMs)} | ${fmtInt(c.medianInputTokens)} / ${fmtInt(c.medianOutputTokens)} | ${fmtPct(c.medianCacheHitRatio)} | ${fmtCost(c.medianCostUsd)} | ${fmtDeltaRatio(c.medianCostUsd, b.medianCostUsd)} |`,
      );
    }
    lines.push("");
  } else {
    const byAgent = groupBy(candidateRecords, (r) => r.agent);
    lines.push("## Overall Summary");
    lines.push("");
    lines.push(
      "| Agent | Model | Runs | Pass Rate | Median Wall | Med In / Cached / Out | Cache Hit | Total Cost | Cost / Solved |",
    );
    lines.push(
      "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    );
    for (const [agentName, aRecords] of byAgent.entries()) {
      const agg = aggregateRecords(aRecords);
      const model = [...new Set(aRecords.map((r) => r.model))].join(", ");
      lines.push(
        `| ${agentName} | ${model} | ${agg.runs} | ${agg.passed}/${agg.runs} (${fmtPct(agg.passRate)}) | ${fmtMs(agg.medianWallMs)} | ${fmtInt(agg.medianInputTokens)} / ${fmtInt(agg.medianCachedTokens)} / ${fmtInt(agg.medianOutputTokens)} | ${fmtPct(agg.medianCacheHitRatio)} | ${fmtCost(agg.totalCostUsd)} | ${fmtCost(agg.costPerSolvedUsd)} |`,
      );
    }
    lines.push("");

    const allTasks = [...new Set(candidateRecords.map((r) => r.task))].sort();
    lines.push("## Per-Task Results");
    lines.push("");
    lines.push(
      "| Task | Agent | Pass | Med Wall | 1st Text | Reqs | Tools (Err) | In / Cached / Out (Med) | Cost (Med) |",
    );
    lines.push(
      "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    );
    for (const taskId of allTasks) {
      for (const [agentName, aRecords] of byAgent.entries()) {
        const subset = aRecords.filter((r) => r.task === taskId);
        if (subset.length === 0) continue;
        const agg = aggregateRecords(subset);
        lines.push(
          `| ${taskId} | ${agentName} | ${agg.passed}/${agg.runs} | ${fmtMs(agg.medianWallMs)} | ${fmtMs(agg.medianFirstTextMs)} | ${fmtInt(agg.medianRequests)} | ${fmtInt(agg.medianToolCalls)} (${agg.totalToolErrors}) | ${fmtInt(agg.medianInputTokens)} / ${fmtInt(agg.medianCachedTokens)} / ${fmtInt(agg.medianOutputTokens)} | ${fmtCost(agg.medianCostUsd)} |`,
        );
      }
    }
    lines.push("");
  }

  // Additive fields of the long-context task (absent in older records).
  const longRuns = candidateRecords.filter(
    (r) => (r.context_window ?? null) !== null || (r.compactions ?? 0) > 0,
  );
  if (longRuns.length > 0) {
    lines.push("## Context Window And Compactions");
    lines.push("");
    lines.push("| Task | Agent | Rep | Window | Compactions | Peak Request Input | Requests | Passed |");
    lines.push("| --- | --- | ---: | ---: | ---: | ---: | ---: | :---: |");
    for (const r of longRuns) {
      lines.push(
        `| ${r.task} | ${r.agent} | ${r.rep} | ${fmtInt(r.context_window ?? null)} | ${fmtInt(r.compactions ?? null)} | ${fmtInt(r.peak_request_input_tokens ?? null)} | ${fmtInt(r.requests)} | ${r.passed ? "yes" : "no"} |`,
      );
    }
    lines.push("");
  }

  const failures = candidateRecords.filter(
    (r) =>
      !r.passed ||
      r.error !== null ||
      r.timed_out ||
      (r.unexplained_cache_misses !== null && r.unexplained_cache_misses > 0),
  );
  if (failures.length > 0) {
    lines.push("## Diagnostics");
    lines.push("");
    for (const r of failures) {
      const tags: string[] = [];
      if (!r.passed) tags.push("FAIL");
      if (r.timed_out) tags.push("TIMEOUT");
      if (r.error) tags.push(`err="${r.error}"`);
      if (r.unexplained_cache_misses) {
        tags.push(`unexplained_cache_misses=${r.unexplained_cache_misses}`);
      }
      const detail = r.check_details ? ` — ${r.check_details.replace(/\r?\n/g, " ")}` : "";
      lines.push(`- \`${r.task}\` (${r.agent}, rep ${r.rep}): ${tags.join(", ")}${detail}`);
    }
    lines.push("");
  }

  return lines.join("\n");
}
