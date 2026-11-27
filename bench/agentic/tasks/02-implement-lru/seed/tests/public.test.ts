import { describe, expect, test } from "bun:test";
import { LruCache } from "../src/lru.ts";

describe("LruCache public tests", () => {
  test("evicts least recently used entry", () => {
    const cache = new LruCache<string, number>({ capacity: 2 });
    cache.set("a", 1);
    cache.set("b", 2);
    expect(cache.get("a")).toBe(1);
    cache.set("c", 3);
    expect(cache.has("b")).toBe(false);
    expect(cache.get("a")).toBe(1);
    expect(cache.get("c")).toBe(3);
  });

  test("expires entries using injected clock", () => {
    let t = 1000;
    const cache = new LruCache<string, string>({
      capacity: 2,
      defaultTtlMs: 50,
      now: () => t,
    });
    cache.set("k", "v");
    expect(cache.get("k")).toBe("v");
    t = 1050;
    expect(cache.get("k")).toBeUndefined();
    expect(cache.stats().expired).toBe(1);
  });
});
