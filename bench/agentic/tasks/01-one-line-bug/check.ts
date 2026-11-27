import assert from "node:assert/strict";
import { importWorkspaceModule, runBunTest } from "../../src/check-helpers.ts";
import type { CheckContext, CheckResult } from "../../src/types.ts";

type Range = [number, number];

export async function check(ctx: CheckContext): Promise<CheckResult> {
  await runBunTest(ctx.workspace);
  const mod = await importWorkspaceModule<{ mergeRanges: (r: readonly Range[]) => Range[] }>(
    ctx.workspace,
    "src/range.ts",
  );

  assert.deepEqual(mod.mergeRanges([]), []);
  assert.deepEqual(mod.mergeRanges([[4, 9]]), [[4, 9]]);
  assert.deepEqual(mod.mergeRanges([[1, 2], [4, 5]]), [[1, 2], [4, 5]]);
  assert.deepEqual(mod.mergeRanges([[1, 3], [3, 5], [5, 8]]), [[1, 8]]);
  assert.deepEqual(mod.mergeRanges([[1, 10], [2, 5], [3, 7]]), [[1, 10]]);
  assert.deepEqual(mod.mergeRanges([[5, 7], [-3, -1], [-2, 0], [0, 2]]), [[-3, 2], [5, 7]]);
  assert.deepEqual(mod.mergeRanges([[2, 4], [2, 4]]), [[2, 4]]);

  const input: Range[] = [[5, 8], [1, 3], [2, 6]];
  const snapshot = JSON.stringify(input);
  const out = mod.mergeRanges(input);
  assert.deepEqual(out, [[1, 8]]);
  assert.equal(JSON.stringify(input), snapshot, "input array must not be mutated");

  return { passed: true, details: "All hidden and public range checks passed." };
}
