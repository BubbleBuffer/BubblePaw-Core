export type BinaryOp = "+" | "-" | "*" | "/";

export type Expr =
  | { kind: "num"; value: number }
  | { kind: "var"; name: string }
  | { kind: "neg"; expr: Expr }
  | { kind: "bin"; op: BinaryOp; left: Expr; right: Expr };

export type Stmt =
  | { kind: "let"; name: string; expr: Expr }
  | { kind: "return"; expr: Expr };

export interface ProgramResult {
  bytecode: string[];
  value: number;
}
