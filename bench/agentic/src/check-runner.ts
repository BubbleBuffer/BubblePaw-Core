import { readFile, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import type { CheckContext, CheckResult } from "./types.ts";

async function main(): Promise<void> {
  const [checkPath, workspace, seedDir, finalTextPath, resultOutPath, taskId] = Bun.argv.slice(2);
  if (!checkPath || !workspace || !seedDir || !finalTextPath || !resultOutPath || !taskId) {
    throw new Error(
      "Usage: check-runner.ts <checkPath> <workspace> <seedDir> <finalTextPath> <resultOutPath> <taskId>",
    );
  }

  const finalText = await readFile(finalTextPath, "utf8");
  let result: CheckResult;
  try {
    const mod = (await import(pathToFileURL(checkPath).href)) as {
      check?: (ctx: CheckContext) => Promise<CheckResult> | CheckResult;
    };
    if (typeof mod.check !== "function") {
      throw new Error(`Check module ${checkPath} does not export a check() function`);
    }
    result = await mod.check({ workspace, seedDir, finalText, taskId });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    result = { passed: false, details: message };
  }

  await writeFile(resultOutPath, JSON.stringify(result), "utf8");
}

await main();
