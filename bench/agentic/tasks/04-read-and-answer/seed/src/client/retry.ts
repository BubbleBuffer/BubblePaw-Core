import type { ClientConfig } from "../config/load.ts";

export function computeBackoffMs(attempt: number, cfg: ClientConfig, rand = Math.random): number {
  const exp = Math.min(cfg.backoffCapMs, cfg.backoffBaseMs * 2 ** attempt);
  return Math.floor(rand() * exp);
}

export async function withRetries<T>(
  cfg: ClientConfig,
  fn: (attempt: number) => Promise<{ ok: boolean; status: number; value?: T }>,
): Promise<T> {
  const retryable = new Set(cfg.retryableStatuses);
  let lastStatus = 0;
  for (let attempt = 0; attempt <= cfg.maxRetries; attempt += 1) {
    const res = await fn(attempt);
    if (res.ok) {
      return res.value as T;
    }
    lastStatus = res.status;
    if (!retryable.has(res.status) || attempt === cfg.maxRetries) {
      throw new Error(`HTTP request failed with status ${lastStatus} after ${attempt + 1} attempts`);
    }
    const delay = computeBackoffMs(attempt, cfg);
    await new Promise((r) => setTimeout(r, delay));
  }
  throw new Error(`HTTP request failed with status ${lastStatus}`);
}
