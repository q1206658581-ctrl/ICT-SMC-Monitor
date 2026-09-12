import { create } from 'zustand';
import { persist } from 'zustand/middleware';

// Sidebar display preference, separate from monitored group/chart order.
export const useSymbolOrderStore = create<{
  order: string[];
  setOrder: (order: string[]) => void;
}>()(
  persist(
    (set) => ({ order: [], setOrder: (order) => set({ order }) }),
    { name: 'ict-radar-symbol-order-v1' },
  ),
);

export function orderedSymbols<T extends { symbol: string }>(rows: T[], order: string[]): T[] {
  const remaining = new Map(rows.map((row) => [row.symbol, row]));
  const sorted: T[] = [];
  for (const symbol of order) {
    const row = remaining.get(symbol);
    if (row) { sorted.push(row); remaining.delete(symbol); }
  }
  // New symbols follow the saved list; removed symbols do not reappear.
  return [...sorted, ...remaining.values()];
}
