export type AccountId = string & { readonly __brand: "AccountId" };

export interface Account {
  id: AccountId;
  currency: string;
}

export interface LedgerEntry {
  id: string;
  accountId: AccountId;
  amountCents: number;
  memo: string;
}
