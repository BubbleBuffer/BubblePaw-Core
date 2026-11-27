Fix the discount calculation bugs in `src/rules.ts` and `src/engine.ts` so `bun test` passes:

1. Use banker's rounding (round half to even) for fractional cents in `roundHalfToEven`.
2. Compute `promoPercent` against the post-tier subtotal (`subtotalCents - tierDiscountCents`), not the original subtotal.
3. Enforce each tier's `maxDiscountCents` cap on the combined discount (`tierDiscountCents + promoDiscountCents`), adjusting `effectivePromo` / `effectiveTier` so `tierDiscountCents + promoDiscountCents === totalDiscountCents` and `finalCents === subtotalCents - totalDiscountCents`.

Hard constraints:
- **Do NOT modify `config/frozen-tiers.json` or `tests/pricing.test.ts`.**
- **Do NOT leave any extra or temporary files in the workspace.**
