export type PositionSide = 'long' | 'short';
export type PriceField = 'entry_price' | 'stop_price' | 'target_price';
export interface PositionPrices {
  side: PositionSide;
  entry_price: number;
  stop_price: number;
  target_price: number | null;
}
export interface UserPosition extends PositionPrices {
  id: string;
  symbol: string;
  source_alert_id: string | null;
  created_at_ts: number;
  updated_at_ts: number;
  drawn_tf: string | null;
  anchor_ts?: number | null;
}
export interface PositionDraft extends PositionPrices {
  symbol: string;
  source_alert_id: string | null;
  drawn_tf: string | null;
  anchor_ts?: number | null;
}
export function validatePrices(p: PositionPrices): string | null {
  if (![p.entry_price, p.stop_price, ...(p.target_price === null ? [] : [p.target_price])].every(Number.isFinite)) return '价格必须是有效数字';
  const direction = p.side === 'long' ? 1 : -1;
  if ((p.entry_price - p.stop_price) * direction <= 0) return '多头止损需低于入场价；空头止损需高于入场价';
  if (p.target_price !== null && (p.target_price - p.entry_price) * direction <= 0) return '多头止盈需高于入场价；空头止盈需低于入场价';
  if (!Number.isFinite(p.entry_price-p.stop_price) || (p.target_price !== null && !Number.isFinite(p.target_price-p.entry_price))) return '价格间距超出范围';
  return null;
}
export function riskReward(p: PositionPrices): string {
  if (p.target_price === null || validatePrices(p)) return '—';
  return (Math.abs(p.target_price-p.entry_price) / Math.abs(p.entry_price-p.stop_price)).toFixed(2);
}
export function movePrices<T extends PositionPrices>(p: T, field: PriceField | 'all', delta: number): T {
  if (field === 'all') return { ...p, entry_price: p.entry_price+delta, stop_price: p.stop_price+delta, target_price: p.target_price === null ? null : p.target_price+delta };
  const original = p[field];
  return original === null ? { ...p } : { ...p, [field]: original+delta };
}
export function defaultPrices(side: PositionSide, entry: number): PositionPrices {
  const gap = Math.max(Math.abs(entry)*0.001, 0.00001);
  const direction = side === 'long' ? 1 : -1;
  return { side, entry_price: entry, stop_price: entry-direction*gap, target_price: entry+direction*gap*2 };
}
export function samePrices(a: PositionPrices, b: PositionPrices): boolean {
  return a.side === b.side && a.entry_price === b.entry_price && a.stop_price === b.stop_price && a.target_price === b.target_price;
}

/** Locate the candle ending at the C2 confirmation instant (timestamps are milliseconds). */
export function historicalAnchorIndex(times: readonly number[], anchor: number, duration: number): number | null {
  if (!Number.isFinite(anchor) || !times.length) return null;
  let low = 0, high = times.length;
  while (low < high) {
    const mid = (low+high) >>> 1;
    if (times[mid] < anchor) low = mid+1; else high = mid;
  }
  const index = low-1;
  return index < 0 || anchor > times[index]+duration ? null : index;
}
