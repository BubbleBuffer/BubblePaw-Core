import type { LedgerEntry } from "./types.ts";
import { parseAccountId } from "./validate.ts";

// Legacy v1 wire format freezes the JSON key "amount_cents" for external consumers.
interface LegacyWirePayload {
  id: string;
  account_id: string;
  amount_cents: number;
  memo: string;
}

export function toLegacyWireJson(entry: LedgerEntry): string {
  const payload: LegacyWirePayload = {
    id: entry.id,
    account_id: entry.accountId,
    amount_cents: entry.amountCents,
    memo: entry.memo,
  };
  return JSON.stringify(payload);
}

export function fromLegacyWireJson(raw: string): LedgerEntry {
  const parsed = JSON.parse(raw) as LegacyWirePayload;
  return {
    id: parsed.id,
    accountId: parseAccountId(parsed.account_id),
    amountCents: parsed.amount_cents,
    memo: parsed.memo,
  };
}
