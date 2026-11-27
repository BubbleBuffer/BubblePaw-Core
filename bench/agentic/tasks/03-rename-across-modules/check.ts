import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import {
  importWorkspaceModule,
  listRelativeFiles,
  runBunTest,
} from "../../src/check-helpers.ts";
import type { CheckContext, CheckResult } from "../../src/types.ts";

export async function check(ctx: CheckContext): Promise<CheckResult> {
  const files = (await listRelativeFiles(ctx.workspace)).filter((f) => f.endsWith(".ts"));
  for (const rel of files) {
    const content = await readFile(join(ctx.workspace, rel), "utf8");
    if (/\bAccountId\b/.test(content)) {
      throw new Error(`Leftover identifier AccountId found in ${rel}`);
    }
    if (/\bamountCents\b/.test(content)) {
      throw new Error(`Leftover identifier amountCents found in ${rel}`);
    }
  }

  const indexContent = await readFile(join(ctx.workspace, "src/index.ts"), "utf8");
  assert.ok(indexContent.includes("LedgerAccountId"), "src/index.ts must export LedgerAccountId");

  await runBunTest(ctx.workspace);

  const mod = await importWorkspaceModule<{
    Ledger: new () => {
      createAccount(id: string, currency?: string): { id: string; currency: string };
      postEntry(entry: { id: string; accountId: string; amountMinor: number; memo: string }): void;
      accountBalance(id: string): number;
    };
    parseAccountId: (raw: string) => string;
    toLegacyWireJson: (entry: {
      id: string;
      accountId: string;
      amountMinor: number;
      memo: string;
    }) => string;
    fromLegacyWireJson: (raw: string) => {
      id: string;
      accountId: string;
      amountMinor: number;
      memo: string;
    };
  }>(ctx.workspace, "src/index.ts");

  const ledger = new mod.Ledger();
  const acct = ledger.createAccount("acct_check");
  ledger.postEntry({
    id: "chk-1",
    accountId: acct.id,
    amountMinor: 550,
    memo: "test",
  });
  assert.equal(ledger.accountBalance(acct.id), 550);

  const wire = mod.toLegacyWireJson({
    id: "w-1",
    accountId: mod.parseAccountId("acct_check"),
    amountMinor: 775,
    memo: "wire-check",
  });
  const parsedWire = JSON.parse(wire) as Record<string, unknown>;
  assert.equal(parsedWire.amount_cents, 775, "Legacy wire JSON key must remain amount_cents");
  assert.equal("amountMinor" in parsedWire, false);
  assert.equal(mod.fromLegacyWireJson(wire).amountMinor, 775);

  return { passed: true, details: "Refactor complete and legacy wire key preserved." };
}
