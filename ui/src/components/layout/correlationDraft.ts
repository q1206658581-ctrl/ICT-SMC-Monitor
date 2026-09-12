import type { PairCorrelation } from '../../types/watchlist';

export type CorrelationDraft = Omit<PairCorrelation, 'direction'> & {
  direction: PairCorrelation['direction'] | '';
};

export function buildCorrelationPairs(symbols: string[], existing: CorrelationDraft[]): CorrelationDraft[] {
  const valid = [...new Set(symbols.map((s) => s.trim()).filter(Boolean))];
  const pairs: CorrelationDraft[] = [];
  for (let i = 0; i < valid.length; i++) {
    for (let j = i + 1; j < valid.length; j++) {
      const [a, b] = [valid[i], valid[j]].sort();
      const match = existing.find((c) => (c.a === a && c.b === b) || (c.a === b && c.b === a));
      pairs.push({ a, b, direction: match?.direction ?? '' });
    }
  }
  return pairs;
}

export function confirmedCorrelations(pairs: CorrelationDraft[]): PairCorrelation[] {
  return pairs.map(({ a, b, direction }) => {
    if (direction !== 'positive' && direction !== 'negative') {
      throw new Error(`请选择 ${a} 与 ${b} 的关系：正相关或负相关`);
    }
    return { a, b, direction };
  });
}
