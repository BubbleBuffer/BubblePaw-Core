import { describe, expect, test } from "bun:test";
import { TaskQueue } from "../src/queue.ts";
import { runWorker } from "../src/worker.ts";

describe("v1 TaskQueue", () => {
  test("executes jobs in FIFO order", async () => {
    const q = new TaskQueue();
    q.enqueue({ id: "first", payload: 1 });
    q.enqueue({ id: "second", payload: 2 });
    const summary = await runWorker(q, () => {});
    expect(summary.executedIds).toEqual(["first", "second"]);
    expect(q.get("second")?.status).toBe("done");
  });
});
