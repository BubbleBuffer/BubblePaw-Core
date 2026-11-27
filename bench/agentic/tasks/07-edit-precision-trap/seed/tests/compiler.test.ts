import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { compile, runProgram, tokenize } from "../src/compiler.ts";

describe("compiler", () => {
  test("diagnostic trace noise before assertions", () => {
    for (let i = 0; i < 110; i += 1) {
      const tokens = tokenize(`let x${i} = ${i} + 1; return x${i};`);
      if (tokens.length === 0) {
        throw new Error("unreachable");
      }
      console.log(`[trace ${String(i).padStart(3, "0")}] tokens=${tokens.length} ok`);
    }
    expect(true).toBe(true);
  });

  test("left-associative subtraction and division", () => {
    expect(runProgram("return 10 - 3 - 2;").value).toBe(5);
    expect(runProgram("return 20 / 4 / 2;").value).toBe(2.5);
  });

  test("unary negation precedence", () => {
    expect(runProgram("return -2 + 3;").value).toBe(1);
    expect(runProgram("return 2 * -3;").value).toBe(-6);
  });

  test("division by zero throws at runtime rather than folding to Infinity", () => {
    expect(() => runProgram("return 10 / 0;")).toThrow("Division by zero");
  });

  test("matches golden bytecode fixture", () => {
    const source = [
      "let a = 10 - 3 - 2;",
      "let b = 20 / 4 / 2;",
      "let c = -a * 2 + b;",
      "return (c - 1) / 2;",
    ].join("\n");
    const expected = readFileSync(
      resolve(import.meta.dir, "../fixtures/expected.txt"),
      "utf8",
    )
      .trim()
      .split(/\r?\n/);
    expect(compile(source)).toEqual(expected);
  });
});
