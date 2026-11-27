import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

interface RatePeriod {
  uncached_input: number;
  cache_read: number | null;
  cache_write: number | null;
  output: number;
}

interface CatalogModel {
  billing?: string;
  periods?: {
    peak?: RatePeriod;
    off_peak?: RatePeriod;
  };
  peak_hours_utc?: {
    weekdays: number[];
    hours: [number, number][];
  };
  long_prompt?: {
    over_tokens: number;
    rates: RatePeriod;
  };
}

interface CatalogFile {
  models?: Record<string, CatalogModel>;
}

const FALLBACK_MODELS: Record<string, CatalogModel> = {
  "deepseek-v4.1-flash": {
    billing: "list_price",
    periods: {
      peak: {
        uncached_input: 0.3,
        cache_read: 0.006,
        cache_write: null,
        output: 1.2,
      },
      off_peak: {
        uncached_input: 0.15,
        cache_read: 0.003,
        cache_write: null,
        output: 0.6,
      },
    },
    peak_hours_utc: {
      weekdays: [0, 1, 2, 3, 4],
      hours: [
        [1, 4],
        [6, 10],
      ],
    },
  },
};

let cachedModels: Record<string, CatalogModel> | null = null;

function loadModels(): Record<string, CatalogModel> {
  if (cachedModels !== null) {
    return cachedModels;
  }
  const here = dirname(fileURLToPath(import.meta.url));
  const catalogPath = resolve(here, "../../../crates/bubblepaw-ai/src/catalog/models.json");
  try {
    const raw = readFileSync(catalogPath, "utf8");
    const parsed = JSON.parse(raw) as CatalogFile;
    if (parsed.models && typeof parsed.models === "object") {
      cachedModels = parsed.models;
      return cachedModels;
    }
  } catch {
    // Fall back to the embedded 2026-10-19 snapshot when run outside the full repo tree.
  }
  cachedModels = FALLBACK_MODELS;
  return cachedModels;
}

export function normalizeModelId(model: string): string {
  const slash = model.lastIndexOf("/");
  return slash >= 0 ? model.slice(slash + 1) : model;
}

function isPeakUtc(model: CatalogModel, at: Date): boolean {
  const schedule = model.peak_hours_utc;
  if (!schedule) {
    return false;
  }
  // Catalog weekdays use 0 = Monday .. 6 = Sunday; JS getUTCDay() uses 0 = Sunday .. 6 = Saturday.
  const mondayZeroWeekday = (at.getUTCDay() + 6) % 7;
  if (!schedule.weekdays.includes(mondayZeroWeekday)) {
    return false;
  }
  const hour = at.getUTCHours();
  return schedule.hours.some(([start, end]) => hour >= start && hour < end);
}

export function estimateCostUsd(
  model: string,
  usage: {
    inputTokens: number | null;
    outputTokens: number | null;
    cachedTokens?: number | null;
    cacheCreationTokens?: number | null;
  },
  atIso?: string,
): number | null {
  if (usage.inputTokens === null || usage.outputTokens === null) {
    return null;
  }
  const models = loadModels();
  const entry = models[normalizeModelId(model)];
  if (!entry || entry.billing !== "list_price" || !entry.periods) {
    return null;
  }

  const cached = usage.cachedTokens ?? 0;
  const cacheWrite = usage.cacheCreationTokens ?? 0;
  if (cached < 0 || cacheWrite < 0 || usage.inputTokens < 0 || usage.outputTokens < 0) {
    return null;
  }
  if (cached + cacheWrite > usage.inputTokens) {
    return null;
  }

  const at = atIso ? new Date(atIso) : new Date();
  const validDate = !Number.isNaN(at.getTime());
  const usePeak = validDate && isPeakUtc(entry, at) && entry.periods.peak !== undefined;

  let rates: RatePeriod | undefined = usePeak
    ? entry.periods.peak
    : (entry.periods.off_peak ?? entry.periods.peak);

  if (entry.long_prompt && usage.inputTokens > entry.long_prompt.over_tokens) {
    rates = entry.long_prompt.rates;
  }
  if (!rates) {
    return null;
  }

  const uncached = usage.inputTokens - cached - cacheWrite;
  if (cached > 0 && rates.cache_read === null) {
    return null;
  }
  if (cacheWrite > 0 && rates.cache_write === null) {
    return null;
  }

  const usd =
    (uncached * rates.uncached_input +
      cached * (rates.cache_read ?? 0) +
      cacheWrite * (rates.cache_write ?? 0) +
      usage.outputTokens * rates.output) /
    1_000_000;

  return Number(usd.toFixed(9));
}
