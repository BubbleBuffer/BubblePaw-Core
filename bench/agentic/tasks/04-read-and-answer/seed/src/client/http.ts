import { loadClientConfig } from "../config/load.ts";
import { CircuitBreaker } from "./breaker.ts";
import { withRetries } from "./retry.ts";

export function createClient(rootDir: string, env: "default" | "staging" | "prod") {
  const cfg = loadClientConfig(rootDir, env);
  const breaker = new CircuitBreaker(cfg);

  return {
    config: cfg,
    async getJson<T>(url: string): Promise<T> {
      if (!breaker.canRequest()) {
        throw new Error("Circuit breaker is OPEN");
      }
      try {
        const value = await withRetries<T>(cfg, async () => {
          const controller = new AbortController();
          const timer = setTimeout(() => controller.abort(), cfg.timeoutMs);
          try {
            const resp = await fetch(url, { signal: controller.signal });
            if (!resp.ok) {
              return { ok: false, status: resp.status };
            }
            return { ok: true, status: resp.status, value: (await resp.json()) as T };
          } finally {
            clearTimeout(timer);
          }
        });
        breaker.recordSuccess();
        return value;
      } catch (err) {
        breaker.recordFailure();
        throw err;
      }
    },
  };
}
