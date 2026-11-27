import { describe, expect, test } from "bun:test";
import { mergeRanges } from "./range.ts";

describe("mergeRanges", () => {
  test("merges overlapping intervals", () => {
    expect(mergeRanges([[1, 4], [2, 6]])).toEqual([[1, 6]]);
  });

  test("keeps disjoint intervals separate", () => {
    expect(mergeRanges([[1, 2], [5, 7]])).toEqual([[1, 2], [5, 7]]);
  });

  test("merges touching intervals", () => {
    expect(mergeRanges([[1, 3], [3, 5]])).toEqual([[1, 5]]);
  });

  test("preserves outer end for nested intervals", () => {
    expect(mergeRanges([[1, 10], [2, 5]])).toEqual([[1, 10]]);
  });
});
