import { loadQueueState, saveQueueState } from "./storage.ts";
import type { EnqueueOptions, JobRecord } from "./types.ts";

export class TaskQueue {
  private readonly jobs = new Map<string, JobRecord>();
  private nextSeq = 0;

  static async open(filePath: string): Promise<TaskQueue> {
    const snapshot = await loadQueueState(filePath);
    const q = new TaskQueue();
    q.nextSeq = snapshot.nextSeq;
    for (const job of snapshot.jobs) {
      q.jobs.set(job.id, { ...job });
    }
    return q;
  }

  async save(filePath: string): Promise<void> {
    await saveQueueState(filePath, {
      nextSeq: this.nextSeq,
      jobs: [...this.jobs.values()],
    });
  }

  enqueue(options: EnqueueOptions): JobRecord {
    if (this.jobs.has(options.id)) {
      throw new Error(`Duplicate job id: ${options.id}`);
    }
    const record: JobRecord = {
      id: options.id,
      seq: this.nextSeq,
      payload: options.payload,
      status: "pending",
    };
    this.nextSeq += 1;
    this.jobs.set(record.id, record);
    return record;
  }

  get(id: string): JobRecord | undefined {
    return this.jobs.get(id);
  }

  list(): JobRecord[] {
    return [...this.jobs.values()];
  }

  nextRunnable(): JobRecord | undefined {
    return [...this.jobs.values()]
      .filter((j) => j.status === "pending")
      .sort((a, b) => a.seq - b.seq)[0];
  }

  markDone(id: string): void {
    const job = this.jobs.get(id);
    if (!job) {
      throw new Error(`Unknown job: ${id}`);
    }
    job.status = "done";
  }
}
