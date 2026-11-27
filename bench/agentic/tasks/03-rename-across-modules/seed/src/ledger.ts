import type { Account, AccountId, LedgerEntry } from "./types.ts";
import { parseAccountId, validateEntry } from "./validate.ts";

export class Ledger {
  private readonly accounts = new Map<AccountId, Account>();
  private readonly entries: LedgerEntry[] = [];

  createAccount(rawId: string, currency = "USD"): Account {
    const id = parseAccountId(rawId);
    if (this.accounts.has(id)) {
      throw new Error(`Account already exists: ${id}`);
    }
    const account: Account = { id, currency };
    this.accounts.set(id, account);
    return account;
  }

  postEntry(entry: LedgerEntry): void {
    if (!this.accounts.has(entry.accountId)) {
      throw new Error(`Unknown account: ${entry.accountId}`);
    }
    validateEntry(entry);
    this.entries.push({ ...entry });
  }

  accountBalance(accountId: AccountId): number {
    let total = 0;
    for (const e of this.entries) {
      if (e.accountId === accountId) {
        total += e.amountCents;
      }
    }
    return total;
  }

  entriesFor(accountId: AccountId): LedgerEntry[] {
    return this.entries.filter((e) => e.accountId === accountId);
  }
}
