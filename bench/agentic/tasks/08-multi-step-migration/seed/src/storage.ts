import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import type { QueueStateSnapshot } from "./types.ts";

export async function loadQueueState(filePath: string): Promise<QueueStateSnapshot> {
  try {
    const raw = await readFile(filePath, "utf8");
    const parsed = JSON.parse(raw) as QueueStateSnapshot;
    return {
      nextSeq: typeof parsed.nextSeq === "number" ? parsed.nextSeq : 0,
      jobs: Array.isArray(parsed.jobs) ? parsed.jobs : [],
    };
  } catch {
    return { nextSeq: 0, jobs: [] };
  }
}

export async function saveQueueState(filePath: string, state: QueueStateSnapshot): Promise<void> {
  await mkdir(dirname(filePath), { recursive: true });
  await writeFile(filePath, JSON.stringify(state, null, 2) + "\n", "utf8");
}
