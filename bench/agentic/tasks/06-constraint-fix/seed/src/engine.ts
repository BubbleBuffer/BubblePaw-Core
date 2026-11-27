import { readFileSync } from "node:fs";
import { join } from "node:path";
import { clampDiscount, roundHalfToEven } from "./rules.ts";

export type TierName = "standard" | "silver" | "gold" | "enterprise";

export interface TierConfig {
  discountPercent: number;
  maxDiscountCents: number;
}

export interface FrozenTiersFile {
  tiers: Record<TierName, TierConfig>;
}

export interface QuoteInput {
  subtotalCents: number;
  tier: TierName;
  promoPercent?: number;
}

export interface QuoteResult {
  subtotalCents: number;
  tierDiscountCents: number;
  promoDiscountCents: number;
  totalDiscountCents: number;
  finalCents: number;
}

export function loadFrozenTiers(rootDir: string): FrozenTiersFile {
  const raw = readFileSync(join(rootDir, "config/frozen-tiers.json"), "utf8");
  return JSON.parse(raw) as FrozenTiersFile;
}

export function computeQuote(input: QuoteInput, tiersFile: FrozenTiersFile): QuoteResult {
  const cfg = tiersFile.tiers[input.tier];
  if (!cfg) {
    throw new Error(`Unknown tier: ${input.tier}`);
  }
  const tierDiscountCents = roundHalfToEven((input.subtotalCents * cfg.discountPercent) / 100);
  const promoPct = input.promoPercent ?? 0;
  // Bug 1: promo is applied to input.subtotalCents instead of post-tier subtotal.
  // Bug 2: maxDiscountCents cap is never enforced via clampDiscount.
  void clampDiscount;
  const promoDiscountCents = roundHalfToEven((input.subtotalCents * promoPct) / 100);
  const totalDiscountCents = tierDiscountCents + promoDiscountCents;

  return {
    subtotalCents: input.subtotalCents,
    tierDiscountCents,
    promoDiscountCents,
    totalDiscountCents,
    finalCents: input.subtotalCents - totalDiscountCents,
  };
}
