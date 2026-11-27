export type Range = [number, number];

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
    // Bug 1: strict `<` fails to merge touching intervals like [1, 3] and [3, 5].
    // Bug 2: overwriting `last[1] = end` shrinks nested intervals like [1, 10] + [2, 5].
    if (start < last[1]) {
      last[1] = end;
    } else {
      merged.push([start, end]);
    }
  }
  return merged;
}
