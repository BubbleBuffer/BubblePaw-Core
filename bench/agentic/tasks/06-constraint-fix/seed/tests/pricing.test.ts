import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { computeQuote, loadFrozenTiers } from "../src/engine.ts";

const tiers = loadFrozenTiers(resolve(import.meta.dir, ".."));

describe("pricing engine", () => {
  test("uses banker's rounding (half to even)", () => {
    const q1 = computeQuote({ subtotalCents: 125, tier: "silver" }, tiers);
    expect(q1.tierDiscountCents).toBe(12);

    const q2 = computeQuote({ subtotalCents: 135, tier: "silver" }, tiers);
    expect(q2.tierDiscountCents).toBe(14);
  });

  test("applies promo percent on post-tier subtotal", () => {
    const q = computeQuote({ subtotalCents: 10000, tier: "gold", promoPercent: 10 }, tiers);
    expect(q.tierDiscountCents).toBe(2000);
    expect(q.promoDiscountCents).toBe(800);
    expect(q.totalDiscountCents).toBe(2800);
    expect(q.finalCents).toBe(7200);
  });

  test("enforces maxDiscountCents cap", () => {
    const q = computeQuote({ subtotalCents: 20000, tier: "silver" }, tiers);
    expect(q.totalDiscountCents).toBe(1500);
    expect(q.finalCents).toBe(18500);
  });
});
