import assert from "node:assert/strict";
import { importWorkspaceModule, runBunTest } from "../../src/check-helpers.ts";
import type { CheckContext, CheckResult } from "../../src/types.ts";

interface LruLike<K, V> {
  readonly capacity: number;
  get(key: K): V | undefined;
  set(key: K, value: V, ttlMs?: number): void;
  has(key: K): boolean;
  delete(key: K): boolean;
  readonly size: number;
  stats(): { hits: number; misses: number; evictions: number; expired: number };
}

export async function check(ctx: CheckContext): Promise<CheckResult> {
  await runBunTest(ctx.workspace);
  const mod = await importWorkspaceModule<{
    LruCache: new <K, V>(opts: {
      capacity: number;
      defaultTtlMs?: number;
      now?: () => number;
    }) => LruLike<K, V>;
  }>(ctx.workspace, "src/lru.ts");

  assert.throws(() => new mod.LruCache({ capacity: 0 }), RangeError);
  assert.throws(() => new mod.LruCache({ capacity: -2 }), RangeError);
  assert.throws(() => new mod.LruCache({ capacity: 1.5 }), RangeError);

  // Capacity 1 + update existing key without eviction.
  const c1 = new mod.LruCache<string, number>({ capacity: 1 });
  c1.set("x", 10);
  c1.set("x", 20);
  assert.equal(c1.get("x"), 20);
  assert.equal(c1.stats().evictions, 0);
  c1.set("y", 30);
  assert.equal(c1.get("x"), undefined);
  assert.equal(c1.get("y"), 30);
  assert.equal(c1.stats().evictions, 1);

  // Purges expired entries before evicting live entries, and has() does not refresh recency.
  let t = 0;
  const c2 = new mod.LruCache<string, string>({ capacity: 2, now: () => t });
  c2.set("old", "1", 10);
  t = 5;
  c2.set("live", "2", 100);
  assert.equal(c2.has("live"), true);
  assert.deepEqual(c2.stats(), { hits: 0, misses: 0, evictions: 0, expired: 0 });
  t = 10;
  c2.set("fresh", "3", 100);
  assert.equal(c2.size, 2);
  assert.equal(c2.stats().evictions, 0);
  assert.equal(c2.stats().expired, 1);
  assert.equal(c2.get("live"), "2");

  // ttlMs <= 0 deletes existing key without eviction; delete() returns false on expired.
  c2.set("live", "updated", 0);
  assert.equal(c2.has("live"), false);
  c2.set("temp", "v", 5);
  t = 20;
  assert.equal(c2.delete("temp"), false);
  assert.equal(c2.stats().expired, 2);

  // stats() returns a copy.
  const snap = c2.stats();
  snap.hits = 9999;
  assert.notEqual(c2.stats().hits, 9999);

  return { passed: true, details: "All hidden and public LRU checks passed." };
}
