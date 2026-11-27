// Reference solution of 09-long-haul, derived from the ground truth (not from the notes).
// Hidden from the agent like check.ts. Used by the fake adapter and by the tests; `breakAt`
// makes it lose one constraint from a given stage on, to show where check.ts reports the loss.
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { buildWorld, finalRows, SUPPLIERS, type SupplierTruth } from "./world.ts";

export const CONSTRAINTS = ["naming", "rounding", "exclusion", "ordering", "minimum", "carry"] as const;
export type Constraint = (typeof CONSTRAINTS)[number];

export interface BreakAt {
  constraint: Constraint;
  fromStage: number;
}

export interface StageFile {
  stage: number;
  suppliers: { code: string; deliveries: number; total_cents: number }[];
  cumulative_cents: number;
}

export function referenceStageFiles(breakAt?: BreakAt): StageFile[] {
  const world = buildWorld();
  const files: StageFile[] = [];
  let cumulative = 0;
  for (const truth of world) {
    const broken = (c: Constraint): boolean =>
      breakAt?.constraint === c && truth.stage >= breakAt.fromStage;
    const minCount = broken("minimum") ? 1 : 2;
    const entries = [...truth.suppliers.values()].filter((t: SupplierTruth) => {
      const blocked = SUPPLIERS.find((s) => s.code === t.code)!.blocked;
      return t.count >= minCount && (!blocked || broken("exclusion"));
    });
    const rows = entries.map((t) => ({
      code: broken("naming") ? `${t.code}-ltd` : t.code,
      deliveries: t.count,
      total_cents: broken("rounding") ? t.up : t.even,
      even: t.even,
    }));
    rows.sort(
      broken("ordering")
        ? (a, b) => (a.code < b.code ? -1 : a.code > b.code ? 1 : 0)
        : (a, b) => b.total_cents - a.total_cents || (a.code < b.code ? -1 : a.code > b.code ? 1 : 0),
    );
    const stageTotal = rows.reduce((acc, r) => acc + r.total_cents, 0);
    cumulative = broken("carry") ? stageTotal : cumulative + stageTotal;
    files.push({
      stage: truth.stage,
      suppliers: rows.map(({ code, deliveries, total_cents }) => ({ code, deliveries, total_cents })),
      cumulative_cents: cumulative,
    });
  }
  return files;
}

export async function writeReference(workspace: string, breakAt?: BreakAt): Promise<number> {
  const files = referenceStageFiles(breakAt);
  const out = join(workspace, "out");
  await mkdir(out, { recursive: true });
  for (const f of files) {
    await writeFile(join(out, `stage-${String(f.stage).padStart(2, "0")}.json`), JSON.stringify(f, null, 2), "utf8");
  }
  const top = finalRows(files.map((f) => f.suppliers));
  await writeFile(join(out, "final.json"), JSON.stringify({ top_suppliers: top }, null, 2), "utf8");
  return files.length + 1;
}

