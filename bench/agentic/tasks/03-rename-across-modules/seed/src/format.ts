import type { Ledger } from "./ledger.ts";
import type { AccountId } from "./types.ts";

export function formatAmount(amountCents: number, currency = "USD"): string {
  const sign = amountCents < 0 ? "-" : "";
  const abs = Math.abs(amountCents);
  const major = Math.floor(abs / 100);
  const minor = String(abs % 100).padStart(2, "0");
  return `${sign}${currency} ${major}.${minor}`;
}

export function formatSummary(ledger: Ledger, accountId: AccountId, currency = "USD"): string {
  const bal = ledger.accountBalance(accountId);
  return `${accountId}: ${formatAmount(bal, currency)}`;
}
