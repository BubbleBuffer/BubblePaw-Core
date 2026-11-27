import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import {
  assertExactFileSet,
  assertFilesUnchanged,
  listRelativeFiles,
} from "../../src/check-helpers.ts";
import type { CheckContext, CheckResult } from "../../src/types.ts";

export async function check(ctx: CheckContext): Promise<CheckResult> {
  const seedFiles = await listRelativeFiles(ctx.seedDir);
  await assertFilesUnchanged(ctx.workspace, ctx.seedDir, seedFiles);
  await assertExactFileSet(ctx.workspace, ctx.seedDir, ["ANSWER.json"]);

  const raw = await readFile(join(ctx.workspace, "ANSWER.json"), "utf8");
  const parsed = JSON.parse(raw) as Record<string, unknown>;

  const expected = {
    prod_timeout_ms: 2500,
    prod_max_total_attempts: 5,
    prod_backoff_base_ms: 120,
    prod_backoff_cap_ms: 4800,
    prod_breaker_open_ms: 45000,
    retryable_statuses: [408, 429, 502, 503, 504],
    superseding_adr: "ADR-004",
  };

  assert.deepEqual(Object.keys(parsed).sort(), Object.keys(expected).sort(), "Exact keys mismatch");
  assert.deepEqual(parsed, expected, "ANSWER.json values do not match effective prod behavior");

  return { passed: true, details: "ANSWER.json exact match and all seed files untouched." };
}
