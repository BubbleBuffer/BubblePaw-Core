# `LruCache<K, V>` Specification

Implement `LruCache<K, V>` in `src/lru.ts` with these exports:

```ts
export interface LruOptions {
  capacity: number;
  defaultTtlMs?: number;
  now?: () => number;
}

export interface LruStats {
  hits: number;
  misses: number;
  evictions: number;
  expired: number;
}
```

## Semantics

1. **Constructor**:
   - Throws `RangeError` if `capacity` is not an integer `>= 1`.
   - Exposes `readonly capacity: number`.
   - `now` defaults to `() => Date.now()`.
2. **Expiration**:
   - An entry with TTL `ttlMs` expires at `expiresAt = now() + ttlMs`.
   - An entry is expired when `now() >= expiresAt`.
3. **`get(key)`**:
   - If missing: increments `misses` and returns `undefined`.
   - If expired: deletes the entry, increments `expired` and `misses`, and returns `undefined`.
   - If live: refreshes recency (moves to most-recently-used), increments `hits`, and returns the value.
4. **`set(key, value, ttlMs?)`**:
   - Effective TTL is `ttlMs !== undefined ? ttlMs : defaultTtlMs`.
   - If effective TTL is defined and `<= 0`: deletes any existing entry for `key` (without counting an eviction) and does not store the key.
   - If `key` already exists: updates value and expiration and refreshes recency without incrementing `evictions`.
   - If `key` is new: first purge all currently expired entries (incrementing `expired` for each removed expired entry). If the cache is still at `capacity`, evict the least-recently-used entry and increment `evictions`. Then insert `key` as most-recently-used.
5. **`has(key)`**:
   - Returns `false` if missing; if expired, deletes it, increments `expired`, and returns `false`.
   - If live, returns `true` **without** refreshing recency and **without** changing `hits` or `misses`.
6. **`delete(key)`**:
   - If missing: returns `false`.
   - If expired: deletes it, increments `expired`, and returns `false`.
   - If live: deletes it and returns `true`.
7. **`get size()`**:
   - Purges expired entries (incrementing `expired` for each) and returns the number of live entries.
8. **`stats()`**:
   - Returns a fresh copy `{ hits, misses, evictions, expired }` so caller mutations do not affect internal counters.
