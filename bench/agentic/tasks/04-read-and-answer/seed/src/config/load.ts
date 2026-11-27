import { readFileSync } from "node:fs";
import { join } from "node:path";

export interface ClientConfig {
  timeoutMs: number;
  maxRetries: number;
  backoffBaseMs: number;
  backoffCapMs: number;
  breakerOpenMs: number;
  retryableStatuses: number[];
}

export function loadClientConfig(rootDir: string, env: "default" | "staging" | "prod"): ClientConfig {
  const base = JSON.parse(
    readFileSync(join(rootDir, "configs/default.json"), "utf8"),
  ) as ClientConfig;
  if (env === "default") {
    return base;
  }
  const override = JSON.parse(
    readFileSync(join(rootDir, `configs/${env}.json`), "utf8"),
  ) as Partial<ClientConfig>;
  return {
    ...base,
    ...override,
    retryableStatuses: override.retryableStatuses
      ? [...override.retryableStatuses].sort((a, b) => a - b)
      : [...base.retryableStatuses].sort((a, b) => a - b),
  };
}
