import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import {
  assertFilesUnchanged,
  importWorkspaceModule,
  runBunTest,
} from "../../src/check-helpers.ts";
import type { CheckContext, CheckResult } from "../../src/types.ts";

export async function check(ctx: CheckContext): Promise<CheckResult> {
  await assertFilesUnchanged(ctx.workspace, ctx.seedDir, [
    "fixtures/expected.txt",
    "tests/compiler.test.ts",
  ]);
  await runBunTest(ctx.workspace);

  const mod = await importWorkspaceModule<{
    compile: (src: string) => string[];
    runProgram: (src: string) => { bytecode: string[]; value: number };
  }>(ctx.workspace, "src/compiler.ts");

  assert.equal(mod.runProgram("return 10 - 3 - 2;").value, 5);
  assert.equal(mod.runProgram("return 20 / 4 / 2;").value, 2.5);
  assert.equal(mod.runProgram("return -2 * 3 + 1;").value, -5);
  assert.equal(mod.runProgram("return --7;").value, 7);
  assert.equal(mod.runProgram("let x = 12; let y = 4; return x - y;").value, 8);
  assert.equal(mod.runProgram("let x = 9; let y = 3; return x / y;").value, 3);
  assert.throws(() => mod.runProgram("return 10 / 0;"), /Division by zero/);

  const goldenSource = [
    "let a = 10 - 3 - 2;",
    "let b = 20 / 4 / 2;",
    "let c = -a * 2 + b;",
    "return (c - 1) / 2;",
  ].join("\n");
  const expectedLines = (
    await readFile(join(ctx.seedDir, "fixtures/expected.txt"), "utf8")
  )
    .trim()
    .split(/\r?\n/);
  assert.deepEqual(mod.compile(goldenSource), expectedLines);
  assert.equal(mod.runProgram(goldenSource).value, -4.25);

  return { passed: true, details: "Compiler bugs fixed and golden bytecode verified." };
}
