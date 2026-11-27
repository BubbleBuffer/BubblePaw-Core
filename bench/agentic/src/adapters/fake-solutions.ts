import { readdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

export interface FakeSolutionSummary {
  text: string;
  toolCalls: number;
  requests: number;
}

export async function applyFakeSolution(
  taskId: string,
  workspace: string,
): Promise<FakeSolutionSummary> {
  switch (taskId) {
    case "01-one-line-bug":
      return solve01OneLineBug(workspace);
    case "02-implement-lru":
      return solve02ImplementLru(workspace);
    case "03-rename-across-modules":
      return solve03RenameAcrossModules(workspace);
    case "04-read-and-answer":
      return solve04ReadAndAnswer(workspace);
    case "05-shell-log-forensics":
      return solve05ShellLogForensics(workspace);
    case "06-constraint-fix":
      return solve06ConstraintFix(workspace);
    case "07-edit-precision-trap":
      return solve07EditPrecisionTrap(workspace);
    case "08-multi-step-migration":
      return solve08MultiStepMigration(workspace);
    default:
      throw new Error(`No canned fake solution registered for task "${taskId}"`);
  }
}

async function solve01OneLineBug(workspace: string): Promise<FakeSolutionSummary> {
  const fixed = `export type Range = [number, number];

export function mergeRanges(ranges: readonly Range[]): Range[] {
  if (ranges.length === 0) {
    return [];
  }
  const sorted: Range[] = ranges
    .map(([start, end]): Range => [start, end])
    .sort((a, b) => a[0] - b[0] || a[1] - b[1]);

  const merged: Range[] = [sorted[0]!];
  for (let i = 1; i < sorted.length; i += 1) {
    const [start, end] = sorted[i]!;
    const last = merged[merged.length - 1]!;
    if (start <= last[1]) {
      last[1] = Math.max(last[1], end);
    } else {
      merged.push([start, end]);
    }
  }
  return merged;
}
`;
  await writeFile(join(workspace, "src/range.ts"), fixed, "utf8");
  return { text: "Fixed touching and nested interval merges in src/range.ts.", toolCalls: 3, requests: 2 };
}

async function solve02ImplementLru(workspace: string): Promise<FakeSolutionSummary> {
  const impl = `export interface LruOptions {
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

interface Entry<V> {
  value: V;
  expiresAt: number | null;
}

export class LruCache<K, V> {
  readonly capacity: number;
  private readonly defaultTtlMs?: number;
  private readonly nowFn: () => number;
  private readonly map = new Map<K, Entry<V>>();
  private hitsCount = 0;
  private missesCount = 0;
  private evictionsCount = 0;
  private expiredCount = 0;

  constructor(options: LruOptions) {
    if (!Number.isInteger(options.capacity) || options.capacity < 1) {
      throw new RangeError("capacity must be an integer >= 1");
    }
    this.capacity = options.capacity;
    this.defaultTtlMs = options.defaultTtlMs;
    this.nowFn = options.now ?? (() => Date.now());
  }

  private isExpired(entry: Entry<V>, now: number): boolean {
    return entry.expiresAt !== null && now >= entry.expiresAt;
  }

  private purgeExpired(now: number): void {
    for (const [k, entry] of this.map.entries()) {
      if (this.isExpired(entry, now)) {
        this.map.delete(k);
        this.expiredCount += 1;
      }
    }
  }

  get(key: K): V | undefined {
    const entry = this.map.get(key);
    if (!entry) {
      this.missesCount += 1;
      return undefined;
    }
    const now = this.nowFn();
    if (this.isExpired(entry, now)) {
      this.map.delete(key);
      this.expiredCount += 1;
      this.missesCount += 1;
      return undefined;
    }
    this.map.delete(key);
    this.map.set(key, entry);
    this.hitsCount += 1;
    return entry.value;
  }

  set(key: K, value: V, ttlMs?: number): void {
    const now = this.nowFn();
    const effectiveTtl = ttlMs !== undefined ? ttlMs : this.defaultTtlMs;
    if (effectiveTtl !== undefined && effectiveTtl <= 0) {
      this.map.delete(key);
      return;
    }
    const expiresAt = effectiveTtl !== undefined ? now + effectiveTtl : null;
    if (this.map.has(key)) {
      this.map.delete(key);
      this.map.set(key, { value, expiresAt });
      return;
    }
    this.purgeExpired(now);
    if (this.map.size >= this.capacity) {
      const oldestKey = this.map.keys().next().value as K;
      this.map.delete(oldestKey);
      this.evictionsCount += 1;
    }
    this.map.set(key, { value, expiresAt });
  }

  has(key: K): boolean {
    const entry = this.map.get(key);
    if (!entry) {
      return false;
    }
    const now = this.nowFn();
    if (this.isExpired(entry, now)) {
      this.map.delete(key);
      this.expiredCount += 1;
      return false;
    }
    return true;
  }

  delete(key: K): boolean {
    const entry = this.map.get(key);
    if (!entry) {
      return false;
    }
    const now = this.nowFn();
    if (this.isExpired(entry, now)) {
      this.map.delete(key);
      this.expiredCount += 1;
      return false;
    }
    return this.map.delete(key);
  }

  get size(): number {
    this.purgeExpired(this.nowFn());
    return this.map.size;
  }

  stats(): LruStats {
    return {
      hits: this.hitsCount,
      misses: this.missesCount,
      evictions: this.evictionsCount,
      expired: this.expiredCount,
    };
  }
}
`;
  await writeFile(join(workspace, "src/lru.ts"), impl, "utf8");
  return { text: "Implemented LruCache in src/lru.ts matching SPEC.md.", toolCalls: 4, requests: 3 };
}

async function solve03RenameAcrossModules(workspace: string): Promise<FakeSolutionSummary> {
  const files = [
    "src/types.ts",
    "src/validate.ts",
    "src/ledger.ts",
    "src/format.ts",
    "src/legacy-wire.ts",
    "src/index.ts",
    "tests/ledger.test.ts",
  ];
  for (const rel of files) {
    const full = join(workspace, rel);
    const content = await readFile(full, "utf8");
    const updated = content
      .replaceAll(/\bAccountId\b/g, "LedgerAccountId")
      .replaceAll(/\bamountCents\b/g, "amountMinor");
    await writeFile(full, updated, "utf8");
  }
  return {
    text: "Renamed AccountId to LedgerAccountId and amountCents to amountMinor while preserving amount_cents wire key.",
    toolCalls: 9,
    requests: 4,
  };
}

async function solve04ReadAndAnswer(workspace: string): Promise<FakeSolutionSummary> {
  const answer = {
    prod_timeout_ms: 2500,
    prod_max_total_attempts: 5,
    prod_backoff_base_ms: 120,
    prod_backoff_cap_ms: 4800,
    prod_breaker_open_ms: 45000,
    retryable_statuses: [408, 429, 502, 503, 504],
    superseding_adr: "ADR-004",
  };
  await writeFile(join(workspace, "ANSWER.json"), JSON.stringify(answer, null, 2) + "\n", "utf8");
  return { text: "Wrote ANSWER.json with verified production retry and breaker settings.", toolCalls: 8, requests: 4 };
}

async function solve05ShellLogForensics(workspace: string): Promise<FakeSolutionSummary> {
  const dataDir = join(workspace, "data");
  const names = (await readdir(dataDir)).filter((f) => f.endsWith(".jsonl")).sort();
  let totalLines = 0;
  let malformedLines = 0;
  let duplicateEvents = 0;
  const seen = new Set<string>();
  const failedTraceSet = new Set<string>();
  const errorCounts: Record<string, number> = {};
  const byService = new Map<string, { durations: number[]; errorCount: number }>();

  for (const name of names) {
    const raw = await readFile(join(dataDir, name), "utf8");
    for (const rawLine of raw.split(/\r?\n/)) {
      if (rawLine.trim().length === 0) {
        continue;
      }
      totalLines += 1;
      let ev: {
        event_id?: unknown;
        service?: unknown;
        status?: unknown;
        duration_ms?: unknown;
        trace_id?: unknown;
        error_code?: unknown;
      };
      try {
        ev = JSON.parse(rawLine);
      } catch {
        malformedLines += 1;
        continue;
      }
      if (
        !ev ||
        typeof ev !== "object" ||
        typeof ev.event_id !== "string" ||
        typeof ev.service !== "string" ||
        typeof ev.status !== "number" ||
        typeof ev.duration_ms !== "number" ||
        typeof ev.trace_id !== "string"
      ) {
        malformedLines += 1;
        continue;
      }
      if (seen.has(ev.event_id)) {
        duplicateEvents += 1;
        continue;
      }
      seen.add(ev.event_id);

      let svc = byService.get(ev.service);
      if (!svc) {
        svc = { durations: [], errorCount: 0 };
        byService.set(ev.service, svc);
      }
      svc.durations.push(ev.duration_ms);
      if (ev.status >= 500) {
        svc.errorCount += 1;
        failedTraceSet.add(ev.trace_id);
        if (typeof ev.error_code === "string" && ev.error_code.length > 0) {
          errorCounts[ev.error_code] = (errorCounts[ev.error_code] ?? 0) + 1;
        }
      }
    }
  }

  const services: Record<string, { count: number; error_count: number; p95_duration_ms: number }> =
    {};
  for (const name of [...byService.keys()].sort()) {
    const svc = byService.get(name)!;
    const sorted = [...svc.durations].sort((a, b) => a - b);
    const idx = Math.ceil(0.95 * sorted.length) - 1;
    services[name] = {
      count: sorted.length,
      error_count: svc.errorCount,
      p95_duration_ms: sorted[idx]!,
    };
  }

  const sortedErrors: Record<string, number> = {};
  for (const key of Object.keys(errorCounts).sort()) {
    sortedErrors[key] = errorCounts[key]!;
  }

  const report = {
    total_lines: totalLines,
    malformed_lines: malformedLines,
    unique_events: seen.size,
    duplicate_events: duplicateEvents,
    failed_trace_ids: [...failedTraceSet].sort(),
    error_counts: sortedErrors,
    services,
  };
  await writeFile(join(workspace, "report.json"), JSON.stringify(report, null, 2) + "\n", "utf8");
  return { text: "Generated report.json from data/*.jsonl.", toolCalls: 5, requests: 3 };
}

async function solve06ConstraintFix(workspace: string): Promise<FakeSolutionSummary> {
  const rules = `export function roundHalfToEven(value: number): number {
  const floor = Math.floor(value);
  const diff = value - floor;
  if (Math.abs(diff - 0.5) < 1e-12) {
    return floor % 2 === 0 ? floor : floor + 1;
  }
  return Math.round(value);
}

export function clampDiscount(discountCents: number, maxDiscountCents: number, subtotalCents: number): number {
  return Math.max(0, Math.min(discountCents, maxDiscountCents, subtotalCents));
}
`;
  const engine = `import { readFileSync } from "node:fs";
import { join } from "node:path";
import { clampDiscount, roundHalfToEven } from "./rules.ts";

export type TierName = "standard" | "silver" | "gold" | "enterprise";

export interface TierConfig {
  discountPercent: number;
  maxDiscountCents: number;
}

export interface FrozenTiersFile {
  tiers: Record<TierName, TierConfig>;
}

export interface QuoteInput {
  subtotalCents: number;
  tier: TierName;
  promoPercent?: number;
}

export interface QuoteResult {
  subtotalCents: number;
  tierDiscountCents: number;
  promoDiscountCents: number;
  totalDiscountCents: number;
  finalCents: number;
}

export function loadFrozenTiers(rootDir: string): FrozenTiersFile {
  const raw = readFileSync(join(rootDir, "config/frozen-tiers.json"), "utf8");
  return JSON.parse(raw) as FrozenTiersFile;
}

export function computeQuote(input: QuoteInput, tiersFile: FrozenTiersFile): QuoteResult {
  const cfg = tiersFile.tiers[input.tier];
  if (!cfg) {
    throw new Error(\`Unknown tier: \${input.tier}\`);
  }
  const tierDiscountCents = roundHalfToEven((input.subtotalCents * cfg.discountPercent) / 100);
  const afterTier = Math.max(0, input.subtotalCents - tierDiscountCents);
  const promoPct = input.promoPercent ?? 0;
  const promoDiscountCents = roundHalfToEven((afterTier * promoPct) / 100);

  const uncappedTotal = tierDiscountCents + promoDiscountCents;
  const totalDiscountCents = clampDiscount(uncappedTotal, cfg.maxDiscountCents, input.subtotalCents);
  const effectiveTier = Math.min(tierDiscountCents, totalDiscountCents);
  const effectivePromo = totalDiscountCents - effectiveTier;

  return {
    subtotalCents: input.subtotalCents,
    tierDiscountCents: effectiveTier,
    promoDiscountCents: effectivePromo,
    totalDiscountCents,
    finalCents: input.subtotalCents - totalDiscountCents,
  };
}
`;
  await writeFile(join(workspace, "src/rules.ts"), rules, "utf8");
  await writeFile(join(workspace, "src/engine.ts"), engine, "utf8");
  return {
    text: "Fixed banker's rounding, post-tier promo base, and tier cap enforcement.",
    toolCalls: 6,
    requests: 3,
  };
}

async function solve07EditPrecisionTrap(workspace: string): Promise<FakeSolutionSummary> {
  const fixedCompiler = `import type { BinaryOp, Expr, ProgramResult, Stmt } from "./ast.ts";

interface Token {
  kind: "num" | "ident" | "op" | "paren" | "semi" | "eq";
  value: string;
}

export function tokenize(source: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  while (i < source.length) {
    const ch = source[i]!;
    if (/\\s/.test(ch)) {
      i += 1;
      continue;
    }
    if (/[0-9]/.test(ch)) {
      let num = "";
      while (i < source.length && /[0-9.]/.test(source[i]!)) {
        num += source[i]!;
        i += 1;
      }
      tokens.push({ kind: "num", value: num });
      continue;
    }
    if (/[a-zA-Z_]/.test(ch)) {
      let id = "";
      while (i < source.length && /[a-zA-Z0-9_]/.test(source[i]!)) {
        id += source[i]!;
        i += 1;
      }
      tokens.push({ kind: "ident", value: id });
      continue;
    }
    if (ch === "+" || ch === "-" || ch === "*" || ch === "/") {
      tokens.push({ kind: "op", value: ch });
      i += 1;
      continue;
    }
    if (ch === "(" || ch === ")") {
      tokens.push({ kind: "paren", value: ch });
      i += 1;
      continue;
    }
    if (ch === ";") {
      tokens.push({ kind: "semi", value: ch });
      i += 1;
      continue;
    }
    if (ch === "=") {
      tokens.push({ kind: "eq", value: ch });
      i += 1;
      continue;
    }
    throw new Error(\`Unexpected character: \${ch}\`);
  }
  return tokens;
}

class Parser {
  private pos = 0;
  constructor(private readonly tokens: Token[]) {}

  private peek(): Token | undefined {
    return this.tokens[this.pos];
  }

  private consume(): Token {
    const tok = this.tokens[this.pos];
    if (!tok) {
      throw new Error("Unexpected end of input");
    }
    this.pos += 1;
    return tok;
  }

  parseProgram(): Stmt[] {
    const stmts: Stmt[] = [];
    while (this.pos < this.tokens.length) {
      stmts.push(this.parseStmt());
      if (this.peek()?.kind === "semi") {
        this.consume();
      }
    }
    return stmts;
  }

  private parseStmt(): Stmt {
    const first = this.peek();
    if (first?.kind === "ident" && first.value === "let") {
      this.consume();
      const nameTok = this.consume();
      if (nameTok.kind !== "ident") {
        throw new Error("Expected identifier after let");
      }
      const eq = this.consume();
      if (eq.kind !== "eq") {
        throw new Error("Expected '=' after variable name");
      }
      return { kind: "let", name: nameTok.value, expr: this.parseAddSub() };
    }
    if (first?.kind === "ident" && first.value === "return") {
      this.consume();
      return { kind: "return", expr: this.parseAddSub() };
    }
    throw new Error(\`Unexpected statement token: \${first?.value ?? "EOF"}\`);
  }

  private parseAddSub(): Expr {
    let left = this.parseMulDiv();
    while (
      this.peek()?.kind === "op" &&
      (this.peek()!.value === "+" || this.peek()!.value === "-")
    ) {
      const op = this.consume().value as BinaryOp;
      const right = this.parseMulDiv();
      left = { kind: "bin", op, left, right };
    }
    return left;
  }

  private parseMulDiv(): Expr {
    let left = this.parseUnary();
    while (
      this.peek()?.kind === "op" &&
      (this.peek()!.value === "*" || this.peek()!.value === "/")
    ) {
      const op = this.consume().value as BinaryOp;
      const right = this.parseUnary();
      left = { kind: "bin", op, left, right };
    }
    return left;
  }

  private parseUnary(): Expr {
    if (this.peek()?.kind === "op" && this.peek()!.value === "-") {
      this.consume();
      return { kind: "neg", expr: this.parseUnary() };
    }
    return this.parsePrimary();
  }

  private parsePrimary(): Expr {
    const tok = this.consume();
    if (tok.kind === "num") {
      return { kind: "num", value: Number(tok.value) };
    }
    if (tok.kind === "ident") {
      return { kind: "var", name: tok.value };
    }
    if (tok.kind === "paren" && tok.value === "(") {
      const inner = this.parseAddSub();
      const close = this.consume();
      if (close.kind !== "paren" || close.value !== ")") {
        throw new Error("Expected closing ')'");
      }
      return inner;
    }
    throw new Error(\`Unexpected token in expression: \${tok.value}\`);
  }
}

const OP_MAP: Record<BinaryOp, { mnemonic: string; evalFn: (a: number, b: number) => number }> = {
  "+": { mnemonic: "ADD", evalFn: (a, b) => a + b },
  "-": { mnemonic: "SUB", evalFn: (a, b) => a - b },
  "*": { mnemonic: "MUL", evalFn: (a, b) => a * b },
  "/": {
    mnemonic: "DIV",
    evalFn: (a, b) => {
      if (b === 0) {
        throw new Error("Division by zero");
      }
      return a / b;
    },
  },
};

function validateExpr(expr: Expr, env: Set<string>): void {
  if (expr.kind === "num") {
    if (!Number.isFinite(expr.value)) {
      throw new Error("Non-finite literal");
    }
    return;
  }
  if (expr.kind === "var") {
    if (!env.has(expr.name)) {
      throw new Error(\`Undefined variable: \${expr.name}\`);
    }
    return;
  }
  if (expr.kind === "neg") {
    validateExpr(expr.expr, env);
    return;
  }
  validateExpr(expr.left, env);
  validateExpr(expr.right, env);
}

function foldExpr(expr: Expr): Expr {
  if (expr.kind === "num" || expr.kind === "var") {
    return expr;
  }
  if (expr.kind === "neg") {
    const inner = foldExpr(expr.expr);
    if (inner.kind === "num") {
      return { kind: "num", value: -inner.value };
    }
    return { kind: "neg", expr: inner };
  }
  const left = foldExpr(expr.left);
  const right = foldExpr(expr.right);
  if (left.kind === "num" && right.kind === "num") {
    if (expr.op === "/" && right.value === 0) {
      return { kind: "bin", op: expr.op, left, right };
    }
    return { kind: "num", value: OP_MAP[expr.op].evalFn(left.value, right.value) };
  }
  return { kind: "bin", op: expr.op, left, right };
}

function emitExpr(expr: Expr, out: string[]): void {
  if (expr.kind === "num") {
    out.push(\`PUSH \${expr.value}\`);
    return;
  }
  if (expr.kind === "var") {
    out.push(\`LOAD \${expr.name}\`);
    return;
  }
  if (expr.kind === "neg") {
    emitExpr(expr.expr, out);
    out.push("NEG");
    return;
  }
  emitExpr(expr.left, out);
  emitExpr(expr.right, out);
  out.push(OP_MAP[expr.op].mnemonic);
}

export function compile(source: string): string[] {
  const stmts = new Parser(tokenize(source)).parseProgram();
  const env = new Set<string>();
  const bytecode: string[] = [];

  for (const stmt of stmts) {
    validateExpr(stmt.expr, env);
    const folded = foldExpr(stmt.expr);
    emitExpr(folded, bytecode);
    if (stmt.kind === "let") {
      bytecode.push(\`STORE \${stmt.name}\`);
      env.add(stmt.name);
    } else {
      bytecode.push("RET");
    }
  }
  return bytecode;
}

export function runBytecode(bytecode: string[]): number {
  const stack: number[] = [];
  const vars = new Map<string, number>();

  for (const instr of bytecode) {
    const [op, arg] = instr.split(" ");
    switch (op) {
      case "PUSH":
        stack.push(Number(arg));
        break;
      case "LOAD":
        stack.push(vars.get(arg!) ?? 0);
        break;
      case "STORE":
        vars.set(arg!, stack.pop()!);
        break;
      case "NEG":
        stack.push(-stack.pop()!);
        break;
      case "ADD": {
        const b = stack.pop()!;
        const a = stack.pop()!;
        stack.push(a + b);
        break;
      }
      case "SUB": {
        const b = stack.pop()!;
        const a = stack.pop()!;
        stack.push(a - b);
        break;
      }
      case "MUL": {
        const b = stack.pop()!;
        const a = stack.pop()!;
        stack.push(a * b);
        break;
      }
      case "DIV": {
        const b = stack.pop()!;
        const a = stack.pop()!;
        if (b === 0) {
          throw new Error("Division by zero");
        }
        stack.push(a / b);
        break;
      }
      case "RET":
        return stack.pop()!;
      default:
        throw new Error(\`Unknown instruction: \${instr}\`);
    }
  }
  throw new Error("Program terminated without RET");
}

export function runProgram(source: string): ProgramResult {
  const bytecode = compile(source);
  const value = runBytecode(bytecode);
  return { bytecode, value };
}
`;
  await writeFile(join(workspace, "src/compiler.ts"), fixedCompiler, "utf8");
  return {
    text: "Fixed left-associativity, unary negation precedence, div-by-zero folding, and stack order in src/compiler.ts.",
    toolCalls: 6,
    requests: 4,
  };
}

async function solve08MultiStepMigration(workspace: string): Promise<FakeSolutionSummary> {
  const typesTs = `export type Priority = "high" | "normal" | "low";

export type JobStatus = "pending" | "done" | "dead" | "blocked";

export interface AttemptRecord {
  attempt: number;
  ok: boolean;
  error?: string;
}

export interface EnqueueOptions {
  id: string;
  payload: unknown;
  priority?: Priority;
  dependsOn?: string[];
  maxAttempts?: number;
}

export interface JobRecord {
  id: string;
  seq: number;
  payload: unknown;
  priority: Priority;
  dependsOn: string[];
  maxAttempts: number;
  attempts: number;
  attemptHistory: AttemptRecord[];
  status: JobStatus;
  lastError?: string;
  blockedBy?: string;
}

export interface QueueStateSnapshot {
  nextSeq: number;
  jobs: JobRecord[];
}
`;

  const storageTs = `import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import type { QueueStateSnapshot } from "./types.ts";

export async function loadQueueState(filePath: string): Promise<QueueStateSnapshot> {
  try {
    const raw = await readFile(filePath, "utf8");
    const parsed = JSON.parse(raw) as QueueStateSnapshot;
    return {
      nextSeq: typeof parsed.nextSeq === "number" ? parsed.nextSeq : 0,
      jobs: Array.isArray(parsed.jobs) ? parsed.jobs : [],
    };
  } catch {
    return { nextSeq: 0, jobs: [] };
  }
}

export async function saveQueueState(filePath: string, state: QueueStateSnapshot): Promise<void> {
  await mkdir(dirname(filePath), { recursive: true });
  const tmpPath = \`\${filePath}.tmp.\${process.pid}.\${Date.now()}.\${Math.random().toString(36).slice(2)}\`;
  await writeFile(tmpPath, JSON.stringify(state, null, 2) + "\\n", "utf8");
  await rename(tmpPath, filePath);
}
`;

  const queueTs = `import { loadQueueState, saveQueueState } from "./storage.ts";
import type { EnqueueOptions, JobRecord, Priority } from "./types.ts";

const PRIORITY_RANK: Record<Priority, number> = {
  high: 0,
  normal: 1,
  low: 2,
};

export class CycleError extends Error {
  readonly cycle: string[];
  constructor(cycle: string[]) {
    super(\`Dependency cycle detected: \${cycle.join(" -> ")}\`);
    this.name = "CycleError";
    this.cycle = cycle;
  }
}

export class TaskQueue {
  private readonly jobs = new Map<string, JobRecord>();
  private nextSeq = 0;

  static async open(filePath: string): Promise<TaskQueue> {
    const snapshot = await loadQueueState(filePath);
    const q = new TaskQueue();
    q.nextSeq = snapshot.nextSeq;
    for (const job of snapshot.jobs) {
      q.jobs.set(job.id, {
        ...job,
        priority: job.priority ?? "normal",
        dependsOn: Array.isArray(job.dependsOn) ? [...job.dependsOn] : [],
        maxAttempts: typeof job.maxAttempts === "number" ? job.maxAttempts : 1,
        attempts: typeof job.attempts === "number" ? job.attempts : 0,
        attemptHistory: Array.isArray(job.attemptHistory) ? [...job.attemptHistory] : [],
      });
    }
    return q;
  }

  async save(filePath: string): Promise<void> {
    await saveQueueState(filePath, {
      nextSeq: this.nextSeq,
      jobs: [...this.jobs.values()].map((j) => ({
        ...j,
        dependsOn: [...j.dependsOn],
        attemptHistory: [...j.attemptHistory],
      })),
    });
  }

  enqueue(options: EnqueueOptions): JobRecord {
    if (this.jobs.has(options.id)) {
      throw new Error(\`Duplicate job id: \${options.id}\`);
    }
    const maxAttempts = options.maxAttempts ?? 1;
    if (!Number.isInteger(maxAttempts) || maxAttempts < 1) {
      throw new RangeError("maxAttempts must be an integer >= 1");
    }
    const dependsOn = [...(options.dependsOn ?? [])];
    for (const depId of dependsOn) {
      if (depId === options.id) {
        throw new CycleError([options.id, options.id]);
      }
      if (!this.jobs.has(depId)) {
        throw new Error(\`Unknown dependency "\${depId}" for job "\${options.id}"\`);
      }
    }

    // Check if adding edges id -> depId creates a cycle (for example via existing edges or self-loop).
    for (const depId of dependsOn) {
      const path = this.findPath(depId, options.id);
      if (path) {
        throw new CycleError([options.id, ...path]);
      }
    }

    let initialStatus: JobRecord["status"] = "pending";
    let blockedBy: string | undefined;
    for (const depId of dependsOn) {
      const dep = this.jobs.get(depId)!;
      if (dep.status === "dead" || dep.status === "blocked") {
        initialStatus = "blocked";
        blockedBy = dep.id;
        break;
      }
    }

    const record: JobRecord = {
      id: options.id,
      seq: this.nextSeq,
      payload: options.payload,
      priority: options.priority ?? "normal",
      dependsOn,
      maxAttempts,
      attempts: 0,
      attemptHistory: [],
      status: initialStatus,
      ...(blockedBy ? { blockedBy } : {}),
    };
    this.nextSeq += 1;
    this.jobs.set(record.id, record);
    return record;
  }

  private findPath(fromId: string, targetId: string): string[] | null {
    const visited = new Set<string>();
    const dfs = (current: string): string[] | null => {
      if (current === targetId) {
        return [current];
      }
      if (visited.has(current)) {
        return null;
      }
      visited.add(current);
      const job = this.jobs.get(current);
      if (!job) {
        return null;
      }
      for (const dep of job.dependsOn) {
        const sub = dfs(dep);
        if (sub) {
          return [current, ...sub];
        }
      }
      return null;
    };
    return dfs(fromId);
  }

  get(id: string): JobRecord | undefined {
    return this.jobs.get(id);
  }

  list(): JobRecord[] {
    return [...this.jobs.values()];
  }

  listRunnable(): JobRecord[] {
    const runnable = [...this.jobs.values()].filter((job) => {
      if (job.status !== "pending") {
        return false;
      }
      return job.dependsOn.every((depId) => this.jobs.get(depId)?.status === "done");
    });
    runnable.sort((a, b) => {
      const prioDiff = PRIORITY_RANK[a.priority] - PRIORITY_RANK[b.priority];
      return prioDiff !== 0 ? prioDiff : a.seq - b.seq;
    });
    return runnable;
  }

  nextRunnable(): JobRecord | undefined {
    return this.listRunnable()[0];
  }

  markDone(id: string): void {
    const job = this.jobs.get(id);
    if (!job) {
      throw new Error(\`Unknown job: \${id}\`);
    }
    job.attempts += 1;
    job.attemptHistory.push({ attempt: job.attempts, ok: true });
    job.status = "done";
  }

  markFailed(id: string, errorMessage: string): void {
    const job = this.jobs.get(id);
    if (!job) {
      throw new Error(\`Unknown job: \${id}\`);
    }
    job.attempts += 1;
    job.lastError = errorMessage;
    job.attemptHistory.push({ attempt: job.attempts, ok: false, error: errorMessage });
    if (job.attempts >= job.maxAttempts) {
      job.status = "dead";
      this.cascadeBlocked(job.id);
    }
  }

  private cascadeBlocked(rootDeadOrBlockedId: string): void {
    const queue: string[] = [rootDeadOrBlockedId];
    while (queue.length > 0) {
      const failedId = queue.shift()!;
      for (const candidate of this.jobs.values()) {
        if (candidate.status === "pending" && candidate.dependsOn.includes(failedId)) {
          candidate.status = "blocked";
          candidate.blockedBy = failedId;
          queue.push(candidate.id);
        }
      }
    }
  }

  statusSummary(): {
    total: number;
    pending: number;
    done: number;
    dead: number;
    blocked: number;
  } {
    let pending = 0;
    let done = 0;
    let dead = 0;
    let blocked = 0;
    for (const job of this.jobs.values()) {
      if (job.status === "pending") pending += 1;
      else if (job.status === "done") done += 1;
      else if (job.status === "dead") dead += 1;
      else if (job.status === "blocked") blocked += 1;
    }
    return { total: this.jobs.size, pending, done, dead, blocked };
  }
}
`;

  const workerTs = `import type { TaskQueue } from "./queue.ts";
import type { JobRecord } from "./types.ts";

export interface WorkerSummary {
  executedIds: string[];
  doneIds: string[];
  deadIds: string[];
  blockedIds: string[];
}

export async function runWorker(
  queue: TaskQueue,
  handler: (job: JobRecord) => Promise<void> | void,
): Promise<WorkerSummary> {
  const executedIds: string[] = [];
  const doneSet = new Set<string>();
  const deadSet = new Set<string>();

  while (true) {
    const job = queue.nextRunnable();
    if (!job) {
      break;
    }
    executedIds.push(job.id);
    try {
      await handler(job);
      queue.markDone(job.id);
      doneSet.add(job.id);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      queue.markFailed(job.id, msg);
      if (job.status === "dead") {
        deadSet.add(job.id);
      }
    }
  }

  const blockedIds = queue
    .list()
    .filter((j) => j.status === "blocked")
    .map((j) => j.id);

  return {
    executedIds,
    doneIds: [...doneSet],
    deadIds: [...deadSet],
    blockedIds,
  };
}
`;

  const cliTs = `import { TaskQueue } from "./queue.ts";
import type { Priority } from "./types.ts";
import { runWorker } from "./worker.ts";

function getFlag(args: string[], flag: string): string | undefined {
  const idx = args.indexOf(flag);
  if (idx >= 0 && idx + 1 < args.length) {
    return args[idx + 1];
  }
  return undefined;
}

async function main(): Promise<void> {
  const args = Bun.argv.slice(2);
  const cmd = args[0];
  const statePath = getFlag(args, "--state") ?? "queue-state.json";

  if (cmd === "enqueue") {
    const id = getFlag(args, "--id");
    if (!id) {
      throw new Error("--id is required");
    }
    const payloadRaw = getFlag(args, "--payload") ?? "{}";
    const payload = JSON.parse(payloadRaw);
    const priority = (getFlag(args, "--priority") ?? "normal") as Priority;
    const depsRaw = getFlag(args, "--depends-on");
    const dependsOn = depsRaw
      ? depsRaw
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean)
      : [];
    const maxAttemptsRaw = getFlag(args, "--max-attempts");
    const maxAttempts = maxAttemptsRaw ? Number(maxAttemptsRaw) : 1;

    const queue = await TaskQueue.open(statePath);
    const job = queue.enqueue({ id, payload, priority, dependsOn, maxAttempts });
    await queue.save(statePath);
    console.log(JSON.stringify(job));
    return;
  }

  if (cmd === "status") {
    const queue = await TaskQueue.open(statePath);
    console.log(
      JSON.stringify({
        summary: queue.statusSummary(),
        jobs: queue.list(),
      }),
    );
    return;
  }

  if (cmd === "run") {
    const mode = getFlag(args, "--mode") ?? "succeed";
    const failIdsRaw = getFlag(args, "--fail-ids") ?? "";
    const failIds = new Set(
      failIdsRaw
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean),
    );
    const queue = await TaskQueue.open(statePath);
    const summary = await runWorker(queue, async (job) => {
      if (mode === "fail-ids" && failIds.has(job.id)) {
        throw new Error(\`Configured failure for \${job.id}\`);
      }
      if (mode === "fail-all") {
        throw new Error(\`Configured failure for \${job.id}\`);
      }
    });
    await queue.save(statePath);
    console.log(JSON.stringify(summary));
    return;
  }

  throw new Error(\`Unknown command: \${cmd ?? "(none)"}\`);
}

await main();
`;

  const testTs = `import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CycleError, TaskQueue } from "../src/queue.ts";
import { runWorker } from "../src/worker.ts";

describe("TaskQueue", () => {
  test("processes priorities and FIFO within priority", async () => {
    const q = new TaskQueue();
    q.enqueue({ id: "low-1", payload: 1, priority: "low" });
    q.enqueue({ id: "norm-1", payload: 2, priority: "normal" });
    q.enqueue({ id: "high-1", payload: 3, priority: "high" });
    q.enqueue({ id: "norm-2", payload: 4, priority: "normal" });

    const summary = await runWorker(q, () => {});
    expect(summary.executedIds).toEqual(["high-1", "norm-1", "norm-2", "low-1"]);
  });

  test("respects dependencies and detects cycles", () => {
    const q = new TaskQueue();
    q.enqueue({ id: "a", payload: 1 });
    q.enqueue({ id: "b", payload: 2, dependsOn: ["a"] });
    expect(() => q.enqueue({ id: "self", payload: 3, dependsOn: ["self"] })).toThrow(CycleError);
    expect(q.list().map((j) => j.id)).toEqual(["a", "b"]);
  });

  test("retries and cascades blocked status on dead jobs", async () => {
    const dir = mkdtempSync(join(tmpdir(), "tq-"));
    try {
      const stateFile = join(dir, "state.json");
      const q = new TaskQueue();
      q.enqueue({ id: "root", payload: 1, maxAttempts: 2 });
      q.enqueue({ id: "child", payload: 2, dependsOn: ["root"] });
      const res = await runWorker(q, (job) => {
        if (job.id === "root") {
          throw new Error("boom");
        }
      });
      await q.save(stateFile);
      expect(res.deadIds).toEqual(["root"]);
      expect(res.blockedIds).toEqual(["child"]);
      const reloaded = await TaskQueue.open(stateFile);
      expect(reloaded.get("root")?.attempts).toBe(2);
      expect(reloaded.get("child")?.status).toBe("blocked");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
`;

  await Promise.all([
    writeFile(join(workspace, "src/types.ts"), typesTs, "utf8"),
    writeFile(join(workspace, "src/storage.ts"), storageTs, "utf8"),
    writeFile(join(workspace, "src/queue.ts"), queueTs, "utf8"),
    writeFile(join(workspace, "src/worker.ts"), workerTs, "utf8"),
    writeFile(join(workspace, "src/cli.ts"), cliTs, "utf8"),
    writeFile(join(workspace, "tests/queue.test.ts"), testTs, "utf8"),
  ]);

  return {
    text: "Upgraded TaskQueue, storage, worker, CLI, and tests for priority lanes, DAG dependencies, retries, and dead/blocked cascade.",
    toolCalls: 14,
    requests: 6,
  };
}
