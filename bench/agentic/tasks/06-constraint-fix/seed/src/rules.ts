// Bug: currently uses standard half-up rounding instead of round-half-to-even.
export function roundHalfToEven(value: number): number {
  return Math.round(value);
}

export function clampDiscount(
  discountCents: number,
  maxDiscountCents: number,
  subtotalCents: number,
): number {
  return Math.max(0, Math.min(discountCents, maxDiscountCents, subtotalCents));
}
