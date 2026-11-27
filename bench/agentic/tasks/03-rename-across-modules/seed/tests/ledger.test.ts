import { describe, expect, test } from "bun:test";
import {
  formatSummary,
  fromLegacyWireJson,
  Ledger,
  parseAccountId,
  toLegacyWireJson,
} from "../src/index.ts";

describe("ledger", () => {
  test("tracks balance and summary", () => {
    const ledger = new Ledger();
    const acct = ledger.createAccount("acct_main");
    ledger.postEntry({
      id: "e1",
      accountId: acct.id,
      amountCents: 1250,
      memo: "deposit",
    });
    ledger.postEntry({
      id: "e2",
      accountId: parseAccountId("acct_main"),
      amountCents: -250,
      memo: "fee",
    });
    expect(ledger.accountBalance(acct.id)).toBe(1000);
    expect(formatSummary(ledger, acct.id)).toBe("acct_main: USD 10.00");
  });

  test("preserves v1 legacy wire format key amount_cents", () => {
    const acctId = parseAccountId("acct_wire");
    const wire = toLegacyWireJson({
      id: "e9",
      accountId: acctId,
      amountCents: 420,
      memo: "wire",
    });
    expect(JSON.parse(wire)).toEqual({
      id: "e9",
      account_id: "acct_wire",
      amount_cents: 420,
      memo: "wire",
    });
    expect(fromLegacyWireJson(wire).amountCents).toBe(420);
  });
});
