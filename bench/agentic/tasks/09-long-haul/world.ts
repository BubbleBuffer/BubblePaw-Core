// The world of 09-long-haul: suppliers, twenty weeks of receiving notes and the ground truth.
// Deterministic: the same SEED always gives the same notes and the same expected outputs, so the
// setup script (which writes the notes) and check.ts (which recomputes the truth) never disagree.

export const STAGES = 20;
export const SEED = 0x091017;
export const LEGAL_SUFFIXES = ["ltd", "gmbh", "co", "inc", "sa"];

export function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
export const int = (r: () => number, n: number): number => Math.floor(r() * n);
export const pick = <T>(r: () => number, items: readonly T[]): T => items[int(r, items.length)]!;

const SURNAMES = [
  "Harlow", "Finch", "Brennan", "Okafor", "Lindqvist", "Marchetti", "Castellan", "Voss", "Ashdown",
  "Pemberton", "Rourke", "Tanaka", "Whitlock", "Abara", "Delacroix", "Nyberg", "Oakhurst",
  "Penhallow", "Quillfeather", "Rasmussen", "Sandoval", "Thorne", "Underhill", "Vance", "Wexford",
  "Yarrow", "Zimmerman", "Bellweather", "Corrigan", "Drummond", "Ellery", "Fairbairn", "Galloway",
  "Hargreaves", "Ingram", "Jessop", "Kingsley", "Langford", "Maddox", "Northcott",
];
const NOUNS = [
  "Cold Storage", "Produce", "Grain", "Dairy", "Fisheries", "Orchards", "Mills", "Provisions",
  "Salt Works", "Bakeries",
];
const SUFFIXES = ["Ltd", "GmbH", "Co", "Inc", "SA"];
const PLACES = [
  "Wexcombe", "Ashgrove", "Brindle", "Cawdor", "Dalmore", "Eastry", "Fennick", "Garrowby",
  "Halloran", "Ivelet", "Jarrow", "Kelmarsh", "Lowick", "Marrick", "Netherby", "Oldcastle",
  "Pickering", "Quarrendon", "Ravensworth", "Selby", "Thirlby", "Ulverston", "Vexford",
  "Whitgift", "Yarnton", "Zennor", "Applegarth", "Bramwell", "Cragside", "Duxford", "Elsdon",
  "Foxholes", "Grassby", "Hexham", "Ingleby", "Jesmond", "Keld", "Lindisfarne", "Morpeth",
  "Nunwick", "Otterburn", "Plenmeller", "Rothbury",
];
const BLOCKED_NAMES = ["Corvane Holdings", "Mistral Quay Ltd", "Dunmore Trading Co"];
export const COMMODITIES = [
  "oats", "barley", "rapeseed", "frozen peas", "salted cod", "butter", "cheese curd", "potatoes",
  "apples", "flour", "rye", "lentils", "smoked haddock", "milk powder", "walnuts",
];

export interface Supplier {
  index: number;
  name: string;
  short: string;
  code: string;
  place: string;
  /** Shorthand is usable in notes from the stage after this one (its glossary is in that stage). */
  aliasStage: number;
  blocked: boolean;
  /** Codes a model would derive from the shorthand when it does not know the mapping. */
  aliasCodes: string[];
}

/** The naming rule of the job: sup- + lowercase words, "&", "and" and legal suffixes dropped. */
export function codeOf(name: string): string {
  const words = name
    .toLowerCase()
    .split(/\s+/)
    .filter((w) => w !== "&" && w !== "and" && !LEGAL_SUFFIXES.includes(w));
  return `sup-${words.join("-")}`;
}

function stripSuffix(name: string): string {
  const words = name.split(" ");
  const last = words[words.length - 1]!.toLowerCase();
  return LEGAL_SUFFIXES.includes(last) ? words.slice(0, -1).join(" ") : name;
}

export function buildSuppliers(): Supplier[] {
  const names: string[] = [];
  for (let i = 0; i < 40; i += 1) {
    const s = SURNAMES[i]!;
    const suffix = SUFFIXES[i % 5]!;
    switch (i % 4) {
      case 0:
        names.push(`${s} & ${SURNAMES[(i + 13) % 40]!} ${suffix}`);
        break;
      case 1:
        names.push(`${s} ${NOUNS[i % 10]!} ${suffix}`);
        break;
      case 2:
        names.push(`${s} and ${SURNAMES[(i + 21) % 40]!} ${suffix}`);
        break;
      default:
        names.push(`${s} ${NOUNS[(i * 3) % 10]!}`);
    }
  }
  names.push(...BLOCKED_NAMES);
  const suppliers = names.map((name, index): Supplier => {
    const place = PLACES[index]!;
    const blocked = index >= 40;
    return {
      index,
      name,
      short: stripSuffix(name),
      code: codeOf(name),
      place,
      aliasStage: blocked ? (index === 42 ? 5 : 1) : index % 3 === 2 ? 5 : 1,
      blocked,
      aliasCodes: [codeOf(place), codeOf(`${place} lot`), codeOf(`the ${place} lot`)],
    };
  });
  if (new Set(suppliers.map((s) => s.code)).size !== suppliers.length) {
    throw new Error("supplier codes are not unique");
  }
  return suppliers;
}

export const SUPPLIERS: Supplier[] = buildSuppliers();

export type Mention = "formal" | "short" | "alias";

export interface Delivery {
  kind: "D";
  id: string;
  supplier: number;
  mention: Mention;
  commodity: string;
  kg10: number;
  mills: number;
  bay: number;
  r: number;
}
export interface Amend {
  kind: "A";
  delivery: Delivery;
  field: "qty" | "price";
  nth: number;
  kg10: number;
  mills: number;
  r: number;
}
export interface Void {
  kind: "V";
  delivery: Delivery;
  r: number;
}
export interface Noise {
  kind: "N";
  sub: "chat" | "sample" | "quote";
  r: number;
  ref?: Delivery;
  supplier: number;
  mention: Mention;
  kg10: number;
  mills: number;
  commodity: string;
}
export type Note = Delivery | Amend | Void | Noise;

export interface SupplierTruth {
  code: string;
  count: number;
  /** Total cents, each delivery rounded half to even (the rule of the job). */
  even: number;
  /** Total cents, each delivery rounded half up (a violation signature). */
  up: number;
  /** Total cents, rounded once at the end (a violation signature). */
  once: number;
}

export interface StageTruth {
  stage: number;
  notes: Note[];
  suppliers: Map<string, SupplierTruth>;
  /** Rows of the stage file in order: not blocked, at least two deliveries. */
  rows: SupplierTruth[];
}

/** x is a price in hundredths of a cent: kg10 * mills. */
export function roundDiv100(x: number, mode: "even" | "up"): number {
  const q = Math.floor(x / 100);
  const rem = x - q * 100;
  if (rem > 50) return q + 1;
  if (rem < 50) return q;
  return mode === "up" || q % 2 !== 0 ? q + 1 : q;
}

export const byTotalThenCode = (a: SupplierTruth, b: SupplierTruth): number =>
  b.even - a.even || (a.code < b.code ? -1 : a.code > b.code ? 1 : 0);

function weights(): number[] {
  return SUPPLIERS.map((s) => (s.blocked ? 3 : 1 + (s.index % 5 === 0 ? 3 : 0)));
}

function pickSupplier(r: () => number, w: number[]): number {
  const total = w.reduce((a, b) => a + b, 0);
  let x = r() * total;
  for (let i = 0; i < w.length; i += 1) {
    x -= w[i]!;
    if (x < 0) return i;
  }
  return w.length - 1;
}

function pickMention(r: () => number, supplier: Supplier, stage: number): Mention {
  const roll = r();
  if (stage > supplier.aliasStage && roll < 0.45) return "alias";
  return roll < 0.7 ? "short" : "formal";
}

const pad2 = (n: number): string => String(n).padStart(2, "0");
const pad3 = (n: number): string => String(n).padStart(3, "0");

function computeTruth(stage: number, notes: Note[]): StageTruth {
  const state = new Map<Delivery, { kg10: number; mills: number; void: boolean }>();
  for (const note of notes) {
    if (note.kind === "D") {
      state.set(note, { kg10: note.kg10, mills: note.mills, void: false });
    } else if (note.kind === "A") {
      const s = state.get(note.delivery)!;
      if (note.field === "qty") s.kg10 = note.kg10;
      else s.mills = note.mills;
    } else if (note.kind === "V") {
      state.get(note.delivery)!.void = true;
    }
  }
  const acc = new Map<string, SupplierTruth & { exact: number }>();
  for (const [d, s] of state) {
    if (s.void) continue;
    const code = SUPPLIERS[d.supplier]!.code;
    const t = acc.get(code) ?? { code, count: 0, even: 0, up: 0, once: 0, exact: 0 };
    const x = s.kg10 * s.mills;
    t.count += 1;
    t.even += roundDiv100(x, "even");
    t.up += roundDiv100(x, "up");
    t.exact += x;
    acc.set(code, t);
  }
  const suppliers = new Map<string, SupplierTruth>();
  for (const [code, t] of acc) {
    suppliers.set(code, { code, count: t.count, even: t.even, up: t.up, once: roundDiv100(t.exact, "even") });
  }
  const rows = [...suppliers.values()]
    .filter((t) => t.count >= 2 && !SUPPLIERS.find((s) => s.code === t.code)!.blocked)
    .sort(byTotalThenCode);
  return { stage, notes, suppliers, rows };
}

function altTotals(t: SupplierTruth): boolean {
  return t.up !== t.even || t.once !== t.even;
}

/** Guarantees that every stage can show each constraint being kept or lost. */
function usable(truth: StageTruth, stage: number): boolean {
  const notBlocked = [...truth.suppliers.values()].filter(
    (t) => !SUPPLIERS.find((s) => s.code === t.code)!.blocked,
  );
  const singles = notBlocked.filter((t) => t.count === 1).length;
  const discriminating = truth.rows.filter(altTotals).length;
  const blocked = [...truth.suppliers.values()].filter((t) => !notBlocked.includes(t));
  const blockedDeliveries = blocked.reduce((a, t) => a + t.count, 0);
  const blockedViaAlias = truth.notes.filter(
    (n) => n.kind === "D" && n.mention === "alias" && SUPPLIERS[n.supplier]!.blocked,
  ).length;
  const viaAlias = truth.notes.filter((n) => n.kind === "D" && n.mention === "alias").length;
  return (
    singles >= 2 &&
    discriminating >= 3 &&
    truth.rows.length >= 12 &&
    blockedDeliveries >= 3 &&
    (stage < 2 || (blockedViaAlias >= 1 && viaAlias >= 4))
  );
}

function buildStage(stage: number): StageTruth {
  const w = weights();
  for (let attempt = 0; attempt < 500; attempt += 1) {
    const r = rng((SEED ^ Math.imul(stage, 7919) ^ Math.imul(attempt, 104729)) >>> 0);
    const nDeliveries = 250 + int(r, 31);
    const deliveries: Delivery[] = [];
    for (let i = 0; i < nDeliveries; i += 1) {
      const si = pickSupplier(r, w);
      deliveries.push({
        kind: "D",
        id: "",
        supplier: si,
        mention: pickMention(r, SUPPLIERS[si]!, stage),
        commodity: pick(r, COMMODITIES),
        kg10: 200 + int(r, 8800),
        mills: 25 * (60 + int(r, 181)),
        bay: 1 + int(r, 9),
        r: int(r, 2 ** 31),
      });
    }
    const noise: Noise[] = [];
    for (let i = 0; i < 100; i += 1) {
      const roll = r();
      const si = pickSupplier(r, w);
      noise.push({
        kind: "N",
        sub: roll < 0.55 ? "chat" : roll < 0.8 ? "sample" : "quote",
        r: int(r, 2 ** 31),
        supplier: si,
        mention: pickMention(r, SUPPLIERS[si]!, stage),
        kg10: 200 + int(r, 8800),
        mills: 25 * (60 + int(r, 181)),
        commodity: pick(r, COMMODITIES),
      });
    }
    // Shuffle deliveries and noise into one sequence, then number the deliveries.
    const seq: Note[] = [...deliveries, ...noise];
    for (let i = seq.length - 1; i > 0; i -= 1) {
      const j = int(r, i + 1);
      [seq[i], seq[j]] = [seq[j]!, seq[i]!];
    }
    let n = 0;
    for (const note of seq) {
      if (note.kind === "D") note.id = `R-${pad2(stage)}-${pad3(++n)}`;
    }
    // Voids and amendments come after their delivery, later in the stage.
    for (const d of deliveries) {
      const events: Note[] = [];
      const roll = r();
      if (roll < 0.07) {
        events.push({ kind: "V", delivery: d, r: int(r, 2 ** 31) });
      } else if (roll < 0.21) {
        const field = r() < 0.5 ? "qty" : "price";
        events.push({ kind: "A", delivery: d, field, nth: 1, kg10: 200 + int(r, 8800), mills: 25 * (60 + int(r, 181)), r: int(r, 2 ** 31) });
        if (r() < 0.25) {
          const second = r() < 0.5 ? "qty" : "price";
          events.push({ kind: "A", delivery: d, field: second, nth: 2, kg10: 200 + int(r, 8800), mills: 25 * (60 + int(r, 181)), r: int(r, 2 ** 31) });
        }
      }
      let pos = seq.indexOf(d);
      for (const e of events) {
        pos = Math.min(seq.length, pos + 1 + int(r, 45));
        seq.splice(pos, 0, e);
      }
    }
    // Samples point at an earlier delivery of the sequence.
    seq.forEach((note, i) => {
      if (note.kind === "N" && note.sub === "sample") {
        const earlier = seq.slice(0, i).filter((x): x is Delivery => x.kind === "D");
        if (earlier.length > 0) note.ref = earlier[int(r, earlier.length)];
      }
    });
    const truth = computeTruth(stage, seq);
    if (usable(truth, stage)) return truth;
  }
  throw new Error(`no usable stage ${stage} within 500 attempts`);
}

let cached: StageTruth[] | null = null;

/** All stages' notes and truth. Pure function of SEED; memoized per process. */
export function buildWorld(): StageTruth[] {
  if (!cached) {
    cached = Array.from({ length: STAGES }, (_, i) => buildStage(i + 1));
  }
  return cached;
}

export interface FinalRow {
  code: string;
  total_cents: number;
  stages_listed: number;
}

/** The final report: per supplier, the sum of its listed totals; top five, ordered as the rows. */
export function finalRows(perStage: { code: string; total_cents: number }[][]): FinalRow[] {
  const acc = new Map<string, FinalRow>();
  for (const rows of perStage) {
    for (const row of rows) {
      const f = acc.get(row.code) ?? { code: row.code, total_cents: 0, stages_listed: 0 };
      f.total_cents += row.total_cents;
      f.stages_listed += 1;
      acc.set(row.code, f);
    }
  }
  return [...acc.values()]
    .sort((a, b) => b.total_cents - a.total_cents || (a.code < b.code ? -1 : a.code > b.code ? 1 : 0))
    .slice(0, 5);
}
