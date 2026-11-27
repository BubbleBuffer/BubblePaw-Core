import { writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { formatMarkdownReport, loadRecords } from "./src/report.ts";

function printHelp(): void {
  console.log(`Usage: bun run bench/agentic/report.ts <results.jsonl> [--baseline <baseline.jsonl>] [--out <report.md>]`);
}

async function main(): Promise<void> {
  const argv = Bun.argv.slice(2);
  let resultsPath: string | undefined;
  let baselinePath: string | undefined;
  let outPath: string | undefined;

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i]!;
    if (arg === "--help" || arg === "-h") {
      printHelp();
      return;
    }
    if (arg === "--baseline") {
      const val = argv[++i];
      if (!val) throw new Error("Missing value for --baseline");
      baselinePath = resolve(val);
    } else if (arg === "--out") {
      const val = argv[++i];
      if (!val) throw new Error("Missing value for --out");
      outPath = resolve(val);
    } else if (!arg.startsWith("-") && !resultsPath) {
      resultsPath = resolve(arg);
    } else {
      throw new Error(`Unexpected argument: ${arg}`);
    }
  }

  if (!resultsPath) {
    printHelp();
    process.exitCode = 1;
    return;
  }

  const candidateRecords = await loadRecords(resultsPath);
  const baselineRecords = baselinePath ? await loadRecords(baselinePath) : undefined;
  const markdown = formatMarkdownReport(candidateRecords, { baselineRecords });

  if (outPath) {
    await writeFile(outPath, markdown, "utf8");
  }
  console.log(markdown);
}

await main();
