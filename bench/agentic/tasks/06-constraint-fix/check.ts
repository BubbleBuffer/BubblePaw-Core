import assert from "node:assert/strict";
import {
  assertExactFileSet,
  assertFilesUnchanged,
  importWorkspaceModule,
  runBunTest,
} from "../../src/check-helpers.ts";
import type { CheckContext, CheckResult } from "../../src/types.ts";

export async function check(ctx: CheckContext): Promise<CheckResult> {
  await assertFilesUnchanged(ctx.workspace, ctx.seedDir, [
    "config/frozen-tiers.json",
    "tests/pricing.test.ts",
  ]);
  await assertExactFileSet(ctx.workspace, ctx.seedDir);

  await runBunTest(ctx.workspace);

  const mod = await importWorkspaceModule<{
    loadFrozenTiers: (root: string) => unknown;
    computeQuote: (
      input: { subtotalCents: number; tier: string; promoPercent?: number },
      tiers: unknown,
    ) => {
      subtotalCents: number;
      tierDiscountCents: number;
      promoDiscountCents: number;
      totalDiscountCents: number;
      finalCents: number;
    };
  }>(ctx.workspace, "src/engine.ts");

  const tiers = mod.loadFrozenTiers(ctx.workspace);

  const r1 = mod.computeQuote({ subtotalCents: 105, tier: "silver" }, tiers);
  assert.equal(r1.tierDiscountCents, 10);
  const r2 = mod.computeQuote({ subtotalCents: 115, tier: "silver" }, tiers);
  assert.equal(r2.tierDiscountCents, 12);

  // Combined tier + promo must still obey tier maxDiscountCents (gold cap 3000).
  const rGoldCap = mod.computeQuote({ subtotalCents: 14000, tier: "gold", promoPercent: 10 }, tiers);
  assert.equal(rGoldCap.totalDiscountCents, 3000);
  assert.equal(rGoldCap.tierDiscountCents + rGoldCap.promoDiscountCents, 3000);
  assert.equal(rGoldCap.finalCents, 11000);

  // Standard tier has cap 0 even with promo.
  const rStd = mod.computeQuote({ subtotalCents: 5000, tier: "standard", promoPercent: 20 }, tiers);
  assert.equal(rStd.totalDiscountCents, 0);
  assert.equal(rStd.finalCents, 5000);

  return { passed: true, details: "Pricing fixed and all immutable constraints respected." };
}
