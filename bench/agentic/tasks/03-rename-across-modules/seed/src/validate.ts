import type { AccountId, LedgerEntry } from "./types.ts";

export function parseAccountId(raw: string): AccountId {
  const trimmed = raw.trim();
  if (!/^acct_[a-z0-9_]+$/.test(trimmed)) {
    throw new Error(`Invalid account id: ${raw}`);
  }
  return trimmed as AccountId;
}

export function validateEntry(entry: LedgerEntry): void {
  if (!Number.isInteger(entry.amountCents)) {
    throw new Error("amountCents must be an integer");
  }
  if (entry.amountCents === 0) {
    throw new Error("amountCents must be non-zero");
  }
}
