import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  importWorkspaceModule,
  runBunScript,
  runBunTest,
} from "../../src/check-helpers.ts";
import type { CheckContext, CheckResult } from "../../src/types.ts";

interface JobLike {
  id: string;
  seq: number;
  priority: string;
  dependsOn: string[];
  maxAttempts: number;
  attempts: number;
  attemptHistory: { attempt: number; ok: boolean; error?: string }[];
  status: string;
}

interface QueueLike {
  save(path: string): Promise<void>;
  enqueue(opts: {
    id: string;
    payload: unknown;
    priority?: "high" | "normal" | "low";
    dependsOn?: string[];
    maxAttempts?: number;
  }): JobLike;
  get(id: string): JobLike | undefined;
  list(): JobLike[];
  statusSummary(): { total: number; pending: number; done: number; dead: number; blocked: number };
}

export async function check(ctx: CheckContext): Promise<CheckResult> {
  await runBunTest(ctx.workspace);

  const qMod = await importWorkspaceModule<{
    CycleError: new (cycle: string[]) => Error;
    TaskQueue: {
      new (): QueueLike;
      open(path: string): Promise<QueueLike>;
    };
  }>(ctx.workspace, "src/queue.ts");
  const wMod = await importWorkspaceModule<{
    runWorker: (
      queue: QueueLike,
      handler: (job: JobLike) => Promise<void> | void,
    ) => Promise<{
      executedIds: string[];
      doneIds: string[];
      deadIds: string[];
      blockedIds: string[];
    }>;
  }>(ctx.workspace, "src/worker.ts");

  assert.equal(typeof qMod.CycleError, "function", "CycleError must be exported from src/queue.ts");

  // 1. Priority + FIFO tie-breaking.
  const q1 = new qMod.TaskQueue();
  q1.enqueue({ id: "l1", payload: 1, priority: "low" });
  q1.enqueue({ id: "n1", payload: 2, priority: "normal" });
  q1.enqueue({ id: "h1", payload: 3, priority: "high" });
  q1.enqueue({ id: "n2", payload: 4, priority: "normal" });
  const s1 = await wMod.runWorker(q1, () => {});
  assert.deepEqual(s1.executedIds, ["h1", "n1", "n2", "l1"]);

  // 2. Diamond DAG + priority break.
  const q2 = new qMod.TaskQueue();
  q2.enqueue({ id: "a", payload: 1, priority: "normal" });
  q2.enqueue({ id: "b", payload: 2, priority: "low", dependsOn: ["a"] });
  q2.enqueue({ id: "c", payload: 3, priority: "high", dependsOn: ["a"] });
  q2.enqueue({ id: "d", payload: 4, priority: "high", dependsOn: ["b", "c"] });
  const s2 = await wMod.runWorker(q2, () => {});
  assert.deepEqual(s2.executedIds, ["a", "c", "b", "d"]);

  // 3. CycleError & unknown dependency without mutation.
  assert.throws(() => q2.enqueue({ id: "self", payload: 0, dependsOn: ["self"] }), qMod.CycleError);
  assert.throws(() => q2.enqueue({ id: "bad", payload: 0, dependsOn: ["missing"] }));
  assert.equal(q2.get("self"), undefined);
  assert.equal(q2.get("bad"), undefined);

  // 4. Retry succeeds on attempt 3.
  const q3 = new qMod.TaskQueue();
  q3.enqueue({ id: "flaky", payload: 1, maxAttempts: 3 });
  let count = 0;
  const s3 = await wMod.runWorker(q3, () => {
    count += 1;
    if (count < 3) throw new Error(`transient ${count}`);
  });
  assert.deepEqual(s3.executedIds, ["flaky", "flaky", "flaky"]);
  assert.deepEqual(s3.doneIds, ["flaky"]);
  assert.equal(q3.get("flaky")?.attempts, 3);
  assert.equal(q3.get("flaky")?.attemptHistory.length, 3);

  // 5. Dead-letter + transitive block cascade + enqueue on dead parent + persistence.
  const tmpDir = await mkdtemp(join(tmpdir(), "t08-chk-"));
  try {
    const stateFile = join(tmpDir, "nested/state.json");
    const q4 = new qMod.TaskQueue();
    q4.enqueue({ id: "root", payload: 1, maxAttempts: 2 });
    q4.enqueue({ id: "child", payload: 2, dependsOn: ["root"] });
    q4.enqueue({ id: "grand", payload: 3, dependsOn: ["child"] });
    const s4 = await wMod.runWorker(q4, (job) => {
      if (job.id === "root") throw new Error("fatal");
    });
    assert.deepEqual(s4.deadIds, ["root"]);
    assert.deepEqual(s4.blockedIds.sort(), ["child", "grand"]);
    q4.enqueue({ id: "late", payload: 4, dependsOn: ["root"] });
    assert.equal(q4.get("late")?.status, "blocked");
    await q4.save(stateFile);

    const reloaded = await qMod.TaskQueue.open(stateFile);
    assert.deepEqual(reloaded.statusSummary(), {
      total: 4,
      pending: 0,
      done: 0,
      dead: 1,
      blocked: 3,
    });

    // 6. CLI end-to-end verification.
    const cliState = join(tmpDir, "cli-state.json");
    const e1 = await runBunScript(ctx.workspace, "src/cli.ts", [
      "enqueue",
      "--state",
      cliState,
      "--id",
      "j1",
      "--payload",
      '{"x":1}',
      "--priority",
      "high",
      "--max-attempts",
      "2",
    ]);
    assert.equal(e1.exitCode, 0, e1.stderr);

    const e2 = await runBunScript(ctx.workspace, "src/cli.ts", [
      "enqueue",
      "--state",
      cliState,
      "--id",
      "j2",
      "--payload",
      '{"x":2}',
      "--depends-on",
      "j1",
    ]);
    assert.equal(e2.exitCode, 0, e2.stderr);

    const r1 = await runBunScript(ctx.workspace, "src/cli.ts", [
      "run",
      "--state",
      cliState,
      "--mode",
      "fail-ids",
      "--fail-ids",
      "j1",
    ]);
    assert.equal(r1.exitCode, 0, r1.stderr);
    const runOut = JSON.parse(r1.stdout.trim()) as { deadIds: string[]; blockedIds: string[] };
    assert.deepEqual(runOut.deadIds, ["j1"]);
    assert.deepEqual(runOut.blockedIds, ["j2"]);

    const st = await runBunScript(ctx.workspace, "src/cli.ts", ["status", "--state", cliState]);
    assert.equal(st.exitCode, 0, st.stderr);
    const statusOut = JSON.parse(st.stdout.trim()) as {
      summary: { dead: number; blocked: number };
    };
    assert.equal(statusOut.summary.dead, 1);
    assert.equal(statusOut.summary.blocked, 1);
  } finally {
    await rm(tmpDir, { recursive: true, force: true });
  }

  return { passed: true, details: "All hidden TaskQueue v2 API and CLI checks passed." };
}
