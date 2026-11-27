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

export class LruCache<K, V> {
  readonly capacity: number;

  constructor(options: LruOptions) {
    this.capacity = options.capacity;
    throw new Error("LruCache not implemented");
  }

  get(_key: K): V | undefined {
    throw new Error("LruCache not implemented");
  }

  set(_key: K, _value: V, _ttlMs?: number): void {
    throw new Error("LruCache not implemented");
  }

  has(_key: K): boolean {
    throw new Error("LruCache not implemented");
  }

  delete(_key: K): boolean {
    throw new Error("LruCache not implemented");
  }

  get size(): number {
    throw new Error("LruCache not implemented");
  }

  stats(): LruStats {
    throw new Error("LruCache not implemented");
  }
}
