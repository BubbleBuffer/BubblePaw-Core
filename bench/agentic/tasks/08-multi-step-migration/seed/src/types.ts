export type JobStatus = "pending" | "done";

export interface EnqueueOptions {
  id: string;
  payload: unknown;
}

export interface JobRecord {
  id: string;
  seq: number;
  payload: unknown;
  status: JobStatus;
}

export interface QueueStateSnapshot {
  nextSeq: number;
  jobs: JobRecord[];
}
