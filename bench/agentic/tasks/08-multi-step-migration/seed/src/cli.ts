import { TaskQueue } from "./queue.ts";
import { runWorker } from "./worker.ts";

function getFlag(args: string[], flag: string): string | undefined {
  const idx = args.indexOf(flag);
  if (idx >= 0 && idx + 1 < args.length) {
    return args[idx + 1];
  }
  return undefined;
}

async function main(): Promise<void> {
  const args = Bun.argv.slice(2);
  const cmd = args[0];
  const statePath = getFlag(args, "--state") ?? "queue-state.json";

  if (cmd === "enqueue") {
    const id = getFlag(args, "--id");
    if (!id) {
      throw new Error("--id is required");
    }
    const payload = JSON.parse(getFlag(args, "--payload") ?? "{}");
    const queue = await TaskQueue.open(statePath);
    const job = queue.enqueue({ id, payload });
    await queue.save(statePath);
    console.log(JSON.stringify(job));
    return;
  }

  if (cmd === "status") {
    const queue = await TaskQueue.open(statePath);
    console.log(JSON.stringify({ jobs: queue.list() }));
    return;
  }

  if (cmd === "run") {
    const queue = await TaskQueue.open(statePath);
    const summary = await runWorker(queue, () => {});
    await queue.save(statePath);
    console.log(JSON.stringify(summary));
    return;
  }

  throw new Error(`Unknown command: ${cmd ?? "(none)"}`);
}

await main();
