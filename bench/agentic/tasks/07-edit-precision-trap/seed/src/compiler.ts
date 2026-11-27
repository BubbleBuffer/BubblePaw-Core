import type { BinaryOp, Expr, ProgramResult, Stmt } from "./ast.ts";

interface Token {
  kind: "num" | "ident" | "op" | "paren" | "semi" | "eq";
  value: string;
}

export function tokenize(source: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  while (i < source.length) {
    const ch = source[i]!;
    if (/\s/.test(ch)) {
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
    throw new Error(`Unexpected character: ${ch}`);
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
    throw new Error(`Unexpected statement token: ${first?.value ?? "EOF"}`);
  }

  private parseAddSub(): Expr {
    // Bug 1: unary '-' at add/sub precedence swallows the entire RHS expression (-2 + 3 => -(2 + 3)).
    if (this.peek()?.kind === "op" && this.peek()!.value === "-") {
      this.consume();
      return { kind: "neg", expr: this.parseAddSub() };
    }
    const left = this.parseMulDiv();
    // Bug 2: recursive call makes '-' right-associative (10 - 3 - 2 => 10 - (3 - 2)).
    if (
      this.peek()?.kind === "op" &&
      (this.peek()!.value === "+" || this.peek()!.value === "-")
    ) {
      const op = this.consume().value as BinaryOp;
      const right = this.parseAddSub();
      return { kind: "bin", op, left, right };
    }
    return left;
  }

  private parseMulDiv(): Expr {
    const left = this.parsePrimary();
    // Bug 3: recursive call makes '/' right-associative (20 / 4 / 2 => 20 / (4 / 2)),
    // and calling parsePrimary directly prevents unary '-' after '*' or '/'.
    if (
      this.peek()?.kind === "op" &&
      (this.peek()!.value === "*" || this.peek()!.value === "/")
    ) {
      const op = this.consume().value as BinaryOp;
      const right = this.parseMulDiv();
      return { kind: "bin", op, left, right };
    }
    return left;
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
    throw new Error(`Unexpected token in expression: ${tok.value}`);
  }
}

function validateExpr(expr: Expr, env: Set<string>): void {
  if (expr.kind === "num") {
    if (!Number.isFinite(expr.value)) {
      throw new Error("Non-finite literal");
    }
    return;
  }
  if (expr.kind === "var") {
    if (!env.has(expr.name)) {
      throw new Error(`Undefined variable: ${expr.name}`);
    }
    return;
  }
  if (expr.kind === "neg") {
    validateExpr(expr.expr, env);
    return;
  }
  switch (expr.op) {
    case "+":
    case "-":
    case "*":
    case "/":
      validateExpr(expr.left, env);
      validateExpr(expr.right, env);
      break;
  }
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
    switch (expr.op) {
      case "+":
        return { kind: "num", value: left.value + right.value };
      case "-":
        return { kind: "num", value: left.value - right.value };
      case "*":
        return { kind: "num", value: left.value * right.value };
      case "/":
        // Bug 4: folds division by zero into Infinity instead of keeping runtime DIV.
        return { kind: "num", value: left.value / right.value };
    }
  }
  return { kind: "bin", op: expr.op, left, right };
}

function emitExpr(expr: Expr, out: string[]): void {
  if (expr.kind === "num") {
    out.push(`PUSH ${expr.value}`);
    return;
  }
  if (expr.kind === "var") {
    out.push(`LOAD ${expr.name}`);
    return;
  }
  if (expr.kind === "neg") {
    emitExpr(expr.expr, out);
    out.push("NEG");
    return;
  }
  switch (expr.op) {
    case "+":
      emitExpr(expr.left, out);
      emitExpr(expr.right, out);
      out.push("ADD");
      break;
    case "-":
      // Bug 5: reversed operand push order on SUB and DIV.
      emitExpr(expr.right, out);
      emitExpr(expr.left, out);
      out.push("SUB");
      break;
    case "*":
      emitExpr(expr.left, out);
      emitExpr(expr.right, out);
      out.push("MUL");
      break;
    case "/":
      emitExpr(expr.right, out);
      emitExpr(expr.left, out);
      out.push("DIV");
      break;
  }
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
      bytecode.push(`STORE ${stmt.name}`);
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
        throw new Error(`Unknown instruction: ${instr}`);
    }
  }
  throw new Error("Program terminated without RET");
}

export function runProgram(source: string): ProgramResult {
  const bytecode = compile(source);
  const value = runBytecode(bytecode);
  return { bytecode, value };
}
