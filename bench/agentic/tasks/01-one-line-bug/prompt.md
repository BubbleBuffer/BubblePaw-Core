Fix `mergeRanges` in `src/range.ts` so that:

1. Overlapping and touching closed intervals (`[1, 3]` and `[3, 5]`) are merged into `[1, 5]`.
2. Nested intervals (`[1, 10]` followed by `[2, 5]`) keep the wider outer end (`[1, 10]`) instead of shrinking it.
3. Unsorted inputs and negative coordinates work correctly, and the input array and its tuples are never mutated.

Run `bun test` to verify your fix.
