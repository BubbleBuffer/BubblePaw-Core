import type { TaskQueue } from "./queue.ts";
import type { JobRecord } from "./types.ts";

export interface WorkerSummary {
  executedIds: string[];
  doneIds: string[];
}

export async function runWorker(
  queue: TaskQueue,
  handler: (job: JobRecord) => Promise<void> | void,
): Promise<WorkerSummary> {
  const executedIds: string[] = [];
  const doneIds: string[] = [];

  while (true) {
    const job = queue.nextRunnable();
    if (!job) {
      break;
    }
    executedIds.push(job.id);
    await handler(job);
    queue.markDone(job.id);
    doneIds.push(job.id);
  }

  return { executedIds, doneIds };
}
