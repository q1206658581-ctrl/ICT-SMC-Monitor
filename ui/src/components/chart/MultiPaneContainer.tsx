import { useEffect, useMemo, useState } from 'react';
import { useChartStore } from '../../store';
import { ChartPane } from './ChartPane';
import {
  loadCommonHistoryWindow,
  peekCommonHistoryWindow,
  refreshCommonHistoryWindow,
  type CommonHistoryWindow,
} from './historyWindow';

// Renders 1-3 ChartPane components depending on the active watchlist.
// In single mode (or 1-symbol watchlist) shows one pane; otherwise
// renders all symbols side-by-side with equal width.
export function MultiPaneContainer() {
  const singleMode = useChartStore((s) => s.singleMode);
  const symbols = useChartStore((s) => s.symbols);
  const singleSymbol = useChartStore((s) => s.symbol);
  const tf = useChartStore((s) => s.tf);
  const barsReloadNonce = useChartStore((s) => s.barsReloadNonce);

  const paneSymbols = useMemo(
    () => singleMode || symbols.length <= 1
      ? [singleSymbol]
      : symbols.slice(0, 3),
    [singleMode, singleSymbol, symbols],
  );
  const symbolsKey = paneSymbols.join('\u0000');
  const requestKey = `${tf}:${barsReloadNonce}:${symbolsKey}`;
  const [loadError, setLoadError] = useState<{ key: string; message: string } | null>(null);
  const [historyWindow, setHistoryWindow] = useState<
    (CommonHistoryWindow & { requestKey: string }) | null
  >(null);

  useEffect(() => {
    let cancelled = false;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    let retryCount = 0;

    const load = async () => {
      const alreadyVisible = peekCommonHistoryWindow(paneSymbols, tf);
      if (!alreadyVisible) useChartStore.getState().setLoadStatus('loading_bars');
      try {
        const loaded = await loadCommonHistoryWindow(paneSymbols, tf);
        if (!cancelled) setHistoryWindow({ ...loaded, requestKey });
      } catch (error) {
        if (cancelled) return;
        console.warn('local multi-pane history load failed', error);
      }

      // Network repair is deliberately detached from the first paint. It may
      // take tens of seconds on a cold TradingView connection, but completed
      // SQLite candles and the currently visible chart remain usable.
      try {
        const refreshed = await refreshCommonHistoryWindow(paneSymbols, tf);
        if (!cancelled) { setHistoryWindow({ ...refreshed, requestKey }); setLoadError(null); }
      } catch (error) {
        if (cancelled) return;
        console.warn('background multi-pane history refresh failed', error);
        setLoadError({ key: requestKey, message: String(error) });
        const delay = Math.min(2_000 * (2 ** retryCount), 20_000);
        retryCount += 1;
        retryTimer = setTimeout(() => { if (!cancelled) void load(); }, delay);
        if (!alreadyVisible) useChartStore.getState().setLoadStatus('ready');
      }
    };
    void load();

    return () => {
      cancelled = true;
      if (retryTimer) clearTimeout(retryTimer);
    };
  }, [paneSymbols, requestKey, tf]);

  const activeWindow = historyWindow?.requestKey === requestKey
    ? historyWindow
    : peekCommonHistoryWindow(paneSymbols, tf);

  return (
    <div className="flex h-full w-full">
      {paneSymbols.map((sym, i) => {
        const rows = activeWindow?.rowsBySymbol[sym];
        const date = (ts: number) => new Date(ts).toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai', hour12: false });
        const range = rows?.length ? `${date(rows[0].ts)} — ${date(rows[rows.length - 1].ts)}` : '';
        const error = loadError?.key === requestKey ? loadError.message : '';
        return <div key={sym} className="flex-1 min-w-0 flex flex-col" style={i > 0 ? { borderLeft: '1px solid var(--border)' } : undefined}>
          <div className="px-2 py-1 text-[10px] text-text-3 bg-bg-1 shrink-0" title={`${sym} · ${tf} · 北京时间\n${range}${error ? `\n更新失败：${error}` : ''}`}>
            <div className="truncate">{sym} · {tf.toUpperCase()} · {rows ? `已加载 ${rows.length} 根` : '正在加载历史…'}</div>
            {range && <div className="truncate">{range}</div>}
            {error && <div className="text-amber-400 truncate" role="status">{rows ? '历史更新失败，保留已加载数据；正在重试' : '暂无可用历史，正在重试'} · {error}</div>}
          </div>
          <div className="flex-1 min-h-0"><ChartPane symbol={sym} paneIndex={i} historyRows={rows ?? null} /></div>
        </div>;
      })}
    </div>
  );
}
