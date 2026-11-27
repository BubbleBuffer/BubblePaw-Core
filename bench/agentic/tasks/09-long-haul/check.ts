// Hidden check of 09-long-haul. Scores per stage and per constraint, so the record shows where
// what was set early was lost, not only pass or fail. check_details is a JSON string:
//   stages_correct          stage numbers whose file equals the expected one exactly
//   stages_missing          stages whose file is absent or not valid JSON
//   constraints_kept_by_stage  per constraint one char per stage: 1 kept, 0 lost, - no file
//   first_lost              per constraint the first stage where it was lost (null: never)
//   final                   final.json against the truth and against the agent's own stage files
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import type { CheckContext, CheckResult } from "../../src/types.ts";
import { CONSTRAINTS, type Constraint } from "./reference.ts";
import { buildWorld, finalRows, STAGES, SUPPLIERS } from "./world.ts";

interface Row {
  code?: unknown;
  deliveries?: unknown;
  total_cents?: unknown;
}
interface StageOut {
  suppliers?: Row[];
  cumulative_cents?: unknown;
  stage?: unknown;
}

const ALL_CODES = new Set(SUPPLIERS.map((s) => s.code));
const BLOCKED_CODES = new Set([
  ...SUPPLIERS.filter((s) => s.blocked).map((s) => s.code),
  ...SUPPLIERS.filter((s) => s.blocked).flatMap((s) => s.aliasCodes),
]);
/** A code that is a canonical one plus a suffix still points at that supplier (for scoring only). */
function resolve(code: unknown): string {
  const c = String(code);
  if (ALL_CODES.has(c)) return c;
  for (const known of ALL_CODES) {
    if (c.startsWith(`${known}-`)) return known;
  }
  return c;
}
const CODE_SHAPE = /^sup-[a-z0-9]+(-[a-z0-9]+)*$/;

async function readJson<T>(path: string): Promise<T | null> {
  try {
    return JSON.parse(await readFile(path, "utf8")) as T;
  } catch {
    return null;
  }
}

const stageName = (n: number): string => `stage-${String(n).padStart(2, "0")}.json`;
const rowsOf = (out: StageOut): Row[] => (Array.isArray(out.suppliers) ? out.suppliers : []);
const num = (v: unknown): number => (typeof v === "number" ? v : Number.NaN);

function cmpRows(a: Row, b: Row): number {
  const ta = num(a.total_cents);
  const tb = num(b.total_cents);
  const ca = String(a.code);
  const cb = String(b.code);
  return tb - ta || (ca < cb ? -1 : ca > cb ? 1 : 0);
}

export async function check(ctx: CheckContext): Promise<CheckResult> {
  const world = buildWorld();
  const outs: (StageOut | null)[] = [];
  for (let n = 1; n <= STAGES; n += 1) {
    outs.push(await readJson<StageOut>(join(ctx.workspace, "out", stageName(n))));
  }

  const kept: Record<Constraint, string[]> = {
    naming: [], rounding: [], exclusion: [], ordering: [], minimum: [], carry: [],
  };
  const stagesCorrect: number[] = [];
  const stagesMissing: number[] = [];
  let ownRunningTotal = 0;

  for (let i = 0; i < STAGES; i += 1) {
    const truth = world[i]!;
    const out = outs[i];
    if (!out || !Array.isArray(out.suppliers)) {
      stagesMissing.push(truth.stage);
      for (const c of CONSTRAINTS) kept[c].push("-");
      continue;
    }
    const rows = rowsOf(out);
    const byCode = new Map(rows.map((r) => [resolve(r.code), r]));
    const mark = (c: Constraint, ok: boolean): void => {
      kept[c].push(ok ? "1" : "0");
    };

    mark("naming", rows.every((r) => typeof r.code === "string" && CODE_SHAPE.test(r.code) && ALL_CODES.has(r.code)));
    mark("exclusion", rows.every((r) => !BLOCKED_CODES.has(String(r.code)) && !BLOCKED_CODES.has(resolve(r.code))));
    mark("ordering", rows.every((r, k) => k === 0 || cmpRows(rows[k - 1]!, r) <= 0));
    const singles = [...truth.suppliers.values()].filter((t) => t.count === 1).map((t) => t.code);
    mark("minimum", rows.every((r) => num(r.deliveries) >= 2) && singles.every((c) => !byCode.has(c)));

    // Rounding: kept when the rows where a wrong rule gives another total carry the right one.
    let right = 0;
    let wrong = 0;
    for (const t of truth.rows) {
      const row = byCode.get(t.code);
      if (!row || (t.up === t.even && t.once === t.even)) continue;
      const total = num(row.total_cents);
      if (total === t.even) right += 1;
      else if (total === t.up || total === t.once) wrong += 1;
    }
    mark("rounding", right > 0 && wrong === 0);

    // Carry: the cumulative figure is the sum of every total the agent itself listed so far.
    ownRunningTotal += rows.reduce((acc, r) => acc + (Number.isFinite(num(r.total_cents)) ? num(r.total_cents) : 0), 0);
    mark("carry", num(out.cumulative_cents) === ownRunningTotal);

    // Exact match with the expected file.
    let expectedCumulative = 0;
    for (let k = 0; k <= i; k += 1) expectedCumulative += world[k]!.rows.reduce((a, t) => a + t.even, 0);
    const exact =
      out.stage === truth.stage &&
      rows.length === truth.rows.length &&
      truth.rows.every(
        (t, k) =>
          rows[k]!.code === t.code && rows[k]!.deliveries === t.count && rows[k]!.total_cents === t.even,
      ) &&
      out.cumulative_cents === expectedCumulative;
    if (exact) stagesCorrect.push(truth.stage);
  }

  const joined = Object.fromEntries(CONSTRAINTS.map((c) => [c, kept[c].join("")]));
  const firstLost = Object.fromEntries(
    CONSTRAINTS.map((c) => {
      const at = kept[c].indexOf("0");
      return [c, at < 0 ? null : at + 1];
    }),
  );

  // Final report: against the truth, and against what the agent's own stage files add up to.
  const final = await readJson<{ top_suppliers?: unknown }>(join(ctx.workspace, "out", "final.json"));
  const expectedFinal = finalRows(world.map((t) => t.rows.map((r) => ({ code: r.code, total_cents: r.even }))));
  const ownFinal = finalRows(
    outs.map((o) =>
      o && Array.isArray(o.suppliers)
        ? o.suppliers.map((r) => ({ code: String(r.code), total_cents: num(r.total_cents) }))
        : [],
    ),
  );
  const same = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b);
  const top = final?.top_suppliers;
  const finalCorrect = same(top, expectedFinal);
  const finalConsistent = same(top, ownFinal);

  const passed = stagesCorrect.length === STAGES && finalCorrect;
  const details = {
    stages_total: STAGES,
    stages_correct: stagesCorrect,
    stages_missing: stagesMissing,
    constraints_kept_by_stage: joined,
    first_lost: firstLost,
    final: { present: final !== null, correct: finalCorrect, consistent_with_own_stages: finalConsistent },
  };
  return { passed, details: JSON.stringify(details) };
}
