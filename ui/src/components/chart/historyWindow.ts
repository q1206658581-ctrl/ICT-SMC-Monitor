import { invoke } from '@tauri-apps/api/core';
import type { BarPayload } from '../../types/ipc';

export function historyLimitForTf(tf: string) {
  if (tf === '1m') return 2500;
  // SMT replay publishes from a common 1100-bar M30/H1 event horizon, while
  // a valid row can reference liquidity that predates its SMT K. Keep enough
  // chart headroom for those white-line endpoints so every Inbox row remains
  // fully auditable instead of being hidden by a stricter 1000-bar UI cutoff.
  if (tf === '30m' || tf === '1h') return 1300;
  return 1000;
}

function normalizeHistory(rows: BarPayload[]) {
  const byTs = new Map<number, BarPayload>();
  for (const row of rows) byTs.set(row.ts, row);
  return Array.from(byTs.values()).sort((a, b) => a.ts - b.ts);
}

export type CommonHistoryWindow = {
  commonStartMs: number;
  rowsBySymbol: Record<string, BarPayload[]>;
};

type SymbolHistory = { symbol: string; rows: BarPayload[] };

const historyWindows = new Map<string, CommonHistoryWindow>();
const refreshes = new Map<string, Promise<CommonHistoryWindow>>();
const refreshedAt = new Map<string, number>();
const PROVIDER_REFRESH_TTL_MS = 60_000;

function windowKey(symbols: string[], tf: string) {
  return `${tf}\u0001${symbols.join('\u0000')}`;
}

function buildCommonHistoryWindow(
  histories: SymbolHistory[],
  tf: string,
): CommonHistoryWindow {
  const missing = histories.find(({ rows }) => rows.length === 0);
  if (missing) {
    throw new Error(`No ${tf} history available for ${missing.symbol}`);
  }
  const shallow = histories.find(({ rows }) => rows.length < 2);
  if (shallow) {
    // A live aggregator can already expose the rolling candle while a cold
    // provider backfill is still unavailable. Never publish that one-candle
    // snapshot as authoritative history: ChartPane.setData would otherwise
    // erase a previously complete series and the common cutoff would hide all
    // Inbox records.
    throw new Error(`Incomplete ${tf} history for ${shallow.symbol}: ${shallow.rows.length} bar`);
  }

  const commonStartMs = Math.max(...histories.map(({ rows }) => rows[0].ts));
  const commonEndMs = Math.min(...histories.map(({ rows }) => rows[rows.length - 1].ts));
  // A disconnected symbol may end before another pane's bounded history
  // starts. Returning rows filtered only by commonStart made the stale panes
  // completely blank. Keep each pane's real history as a visible diagnostic
  // fallback until the union feed repairs the gap; once ranges overlap, use
  // the canonical shared calendar intersection again.
  if (commonStartMs > commonEndMs) {
    console.warn('No overlapping multi-pane history window', {
      tf,
      ranges: histories.map(({ symbol, rows }) => ({
        symbol,
        from: rows[0].ts,
        to: rows[rows.length - 1].ts,
      })),
    });
    return {
      commonStartMs: Math.min(...histories.map(({ rows }) => rows[0].ts)),
      rowsBySymbol: Object.fromEntries(
        histories.map(({ symbol, rows }) => [symbol, rows]),
      ),
    };
  }

  return {
    commonStartMs,
    rowsBySymbol: Object.fromEntries(
      histories.map(({ symbol, rows }) => [
        symbol,
        rows.filter((row) => row.ts >= commonStartMs && row.ts <= commonEndMs),
      ]),
    ),
  };
}

function rememberWindow(symbols: string[], tf: string, window: CommonHistoryWindow) {
  historyWindows.set(windowKey(symbols, tf), window);
  return window;
}

export function peekCommonHistoryWindow(
  symbols: string[],
  tf: string,
): CommonHistoryWindow | null {
  return historyWindows.get(windowKey(symbols, tf)) ?? null;
}

/**
 * Read the complete SQLite snapshot only. This path never waits for a
 * TradingView connection, so a cold timeframe switch can paint immediately.
 */
export async function loadCommonHistoryWindow(
  symbols: string[],
  tf: string,
): Promise<CommonHistoryWindow> {
  const limit = historyLimitForTf(tf);
  const histories = await Promise.all(
    symbols.map(async (symbol) => ({
      symbol,
      rows: normalizeHistory(
        await invoke<BarPayload[]>('get_cached_history', { symbol, tf, limit }),
      ),
    })),
  );
  return rememberWindow(symbols, tf, buildCommonHistoryWindow(histories, tf));
}

/**
 * Refresh SQLite from TradingView without holding up rendering. Provider
 * calls stay serial because the authenticated account allows only one
 * history session beside the live union feed. A short TTL and single-flight
 * guard prevent rapid TF toggles from starting duplicate repair queues.
 */
export function refreshCommonHistoryWindow(
  symbols: string[],
  tf: string,
): Promise<CommonHistoryWindow> {
  const key = windowKey(symbols, tf);
  const cached = historyWindows.get(key);
  const lastRefresh = refreshedAt.get(key) ?? 0;
  if (cached && Date.now() - lastRefresh < PROVIDER_REFRESH_TTL_MS) {
    return Promise.resolve(cached);
  }
  const active = refreshes.get(key);
  if (active) return active;

  const request = (async () => {
    const limit = historyLimitForTf(tf);
    const histories: SymbolHistory[] = [];
    for (const symbol of symbols) {
      histories.push({
        symbol,
        rows: normalizeHistory(
          await invoke<BarPayload[]>('get_history', { symbol, tf, limit }),
        ),
      });
    }
    const window = rememberWindow(
      symbols,
      tf,
      buildCommonHistoryWindow(histories, tf),
    );
    refreshedAt.set(key, Date.now());
    return window;
  })();
  refreshes.set(key, request);
  void request.finally(() => {
    if (refreshes.get(key) === request) refreshes.delete(key);
  }).catch(() => undefined);
  return request;
}
