import { useEffect, useRef } from 'react';
import { applyDelayedClose } from './finalizedCandle';
import {
  CandlestickSeries,
  ColorType,
  CrosshairMode,
  LineStyle,
  PriceLineSource,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type TickMarkType,
  type Time,
} from 'lightweight-charts';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { useChartStore, useDetectorFilters } from '../../store';
import type { BarPayload } from '../../types/ipc';
import type { IctStructure, StructureEvent } from '../../types/structures';
import { StructureOverlay, DEFAULT_FILTER } from './overlay/StructureOverlay';

function cssVar(name: string, fallback: string) {
  if (typeof window === 'undefined') return fallback;
  const v = getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
  return v || fallback;
}

function toCandle(b: BarPayload) {
  return {
    time: Math.floor(b.ts / 1000) as Time,
    open: b.open,
    high: b.high,
    low: b.low,
    close: b.close,
  };
}

const RIGHT_OFFSET_BARS = 8;

function historyLimitForTf(tf: string) {
  if (tf === '1m') return 2500;
  return 1000;
}

function priceFormatForSymbol(symbol: string) {
  if (symbol.includes('BTC')) return { type: 'price' as const, precision: 2, minMove: 0.01 };
  return { type: 'price' as const, precision: 5, minMove: 0.00001 };
}

function dateFromLwcTime(time: Time) {
  if (typeof time === 'number') return new Date(time * 1000);
  if (typeof time === 'string') return new Date(time);
  return new Date(Date.UTC(time.year, time.month - 1, time.day));
}

// ICT concepts (PDH/PDL, sessions, kill zones) are all NY-time based.
// Display the axis + crosshair in Beijing time (Asia/Shanghai).
const DISPLAY_TZ = 'Asia/Shanghai';
const axisDateFormatter = new Intl.DateTimeFormat('en-US', { month: '2-digit', day: '2-digit', timeZone: DISPLAY_TZ });
const axisTimeFormatter = new Intl.DateTimeFormat('en-US', { hour: '2-digit', minute: '2-digit', hour12: false, timeZone: DISPLAY_TZ });
const crosshairTimeFormatter = new Intl.DateTimeFormat('en-US', {
  month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false, timeZone: DISPLAY_TZ,
});

function tickMarkFormatter(time: Time, tickMarkType: TickMarkType) {
  const date = dateFromLwcTime(time);
  if (tickMarkType === 0 || tickMarkType === 1 || tickMarkType === 2) return axisDateFormatter.format(date);
  return axisTimeFormatter.format(date);
}

export function ChartView() {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<'Candlestick'> | null>(null);
  const overlayRef = useRef<StructureOverlay | null>(null);
 const lastTsRef = useRef<number>(0);
  const loadedHistoryTargetRef = useRef<string | null>(null);

  const symbol = useChartStore((s) => s.symbol);
  const tf = useChartStore((s) => s.tf);
  const watchlistId = useChartStore((s) => s.activeWatchlist?.id);
  const applyBar = useChartStore((s) => s.applyBar);
  const structuresReloadNonce = useChartStore((s) => s.structuresReloadNonce);
  const barsReloadNonce = useChartStore((s) => s.barsReloadNonce);
  const filters = useDetectorFilters();

  // create chart once
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const chart = createChart(el, {
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: cssVar('--bg-0', '#0a0b0d') },
        textColor: cssVar('--text-2', '#a0a6ad'),
        fontFamily: 'JetBrains Mono, ui-monospace, monospace',
        fontSize: 11,
      },
      localization: {
        locale: navigator.language,
        timeFormatter: (time: Time) => crosshairTimeFormatter.format(dateFromLwcTime(time)),
      },
      grid: {
        vertLines: { color: cssVar('--border', '#2a2e33'), style: 1 },
        horzLines: { color: cssVar('--border', '#2a2e33'), style: 1 },
      },
      handleScale: {
        axisPressedMouseMove: {
          time: true,
          price: true,
        },
      },
      rightPriceScale: {
        borderColor: cssVar('--border', '#2a2e33'),
        autoScale: true,
      },
      timeScale: {
        borderColor: cssVar('--border', '#2a2e33'),
        timeVisible: true,
        secondsVisible: false,
        rightOffset: RIGHT_OFFSET_BARS,
        tickMarkFormatter,
      },
      crosshair: {
        mode: CrosshairMode.Normal,
        vertLine: { color: cssVar('--text-3', '#6b7178'), labelBackgroundColor: cssVar('--bg-3', '#22262a') },
        horzLine: { color: cssVar('--text-3', '#6b7178'), labelBackgroundColor: cssVar('--bg-3', '#22262a') },
      },
    });
    const series = chart.addSeries(CandlestickSeries, {
      upColor: cssVar('--bull', '#26a69a'),
      downColor: cssVar('--bear', '#ef5350'),
      borderUpColor: cssVar('--bull', '#26a69a'),
      borderDownColor: cssVar('--bear', '#ef5350'),
      wickUpColor: cssVar('--bull', '#26a69a'),
      wickDownColor: cssVar('--bear', '#ef5350'),
      priceFormat: priceFormatForSymbol(symbol),
      lastValueVisible: true,
      priceLineVisible: true,
      priceLineSource: PriceLineSource.LastBar,
      priceLineStyle: LineStyle.Dashed,
      priceLineWidth: 1,
      priceLineColor: cssVar('--accent', '#4f8cff'),
    });
    chartRef.current = chart;
    seriesRef.current = series;
    overlayRef.current = new StructureOverlay(chart, series, el, symbol, tf);
    overlayRef.current.setFilter({ ...DEFAULT_FILTER, ...filters });
    const syncVisibleRange = () => {
      const range = chart.timeScale().getVisibleRange();
      overlayRef.current?.setVisibleTimeRange(range ? Number(range.from) : undefined, range ? Number(range.to) : undefined);
    };
    chart.timeScale().subscribeVisibleTimeRangeChange(syncVisibleRange);
    chart.timeScale().subscribeVisibleLogicalRangeChange(syncVisibleRange);
    // Expose for ad-hoc inspection from devtools / shell-driven log
    // forwarding: `__ovl.dump()` writes a snapshot of every internal
    // map (prims / priceLines / markers / allStructures) to console,
    // which uiLog mirrors into /tmp/ict-radar-ui.log.
    (window as unknown as { __ovl?: unknown }).__ovl = overlayRef.current;
    return () => {
      chart.timeScale().unsubscribeVisibleTimeRangeChange(syncVisibleRange);
      chart.timeScale().unsubscribeVisibleLogicalRangeChange(syncVisibleRange);
      overlayRef.current?.dispose();
      overlayRef.current = null;
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Push filter changes into overlay.
  useEffect(() => {
    overlayRef.current?.setFilter(filters);
  }, [filters]);

  // Price format is applied inside the data-loading effect below, right
  // before setData, so the scale never shows the previous symbol's prices
  // with the new symbol's precision (e.g. EURUSD 1.14326 shown with BTC
  // precision 2 → "1.14"). See load().

  // Load history + structures snapshot when symbol/tf change.
  useEffect(() => {
    let cancelled = false;
    const limit = historyLimitForTf(tf);

    overlayRef.current?.setTarget(symbol, tf);

    async function load() {
      try {
        const rows = await invoke<BarPayload[]>('get_history', {
          symbol, tf, limit,
        });
        if (cancelled || !seriesRef.current) return;
        const byTs = new Map<number, BarPayload>();
        for (const r of rows) byTs.set(r.ts, r);
        const data = Array.from(byTs.values()).sort((a, b) => a.ts - b.ts).map(toCandle);
        const chart = chartRef.current;
        const targetKey = `${symbol}\u0000${tf}`;
        const isRefresh = loadedHistoryTargetRef.current === targetKey;
        const savedTimeRange = isRefresh ? chart?.timeScale().getVisibleRange() : null;
        const savedLogicalRange = isRefresh ? chart?.timeScale().getVisibleLogicalRange() : null;
        // setData first, then update price format + fit. Calling applyOptions
        // (priceFormat) BEFORE setData was causing the chart to recalculate
        // its price scale against the OLD data with the NEW precision, which
        // left the visible range pinned to the previous symbol's price level
        // — making the new candles invisible until a manual reload.
        seriesRef.current.setData(data);
        seriesRef.current.applyOptions({ priceFormat: priceFormatForSymbol(symbol) });
        if (!isRefresh) chart?.priceScale('right').applyOptions({ autoScale: true });
        lastTsRef.current = data.length ? Number(data[data.length - 1].time) : 0;
        overlayRef.current?.setBarTimes(data.map((c) => ({ time: Number(c.time), high: c.high, low: c.low })));
        if (rows.length) {
          const lastMs = rows.reduce((m, r) => (r.ts > m ? r.ts : m), 0);
          overlayRef.current?.setLastBarTs(lastMs);
        }
        if (!isRefresh) {
          chart?.timeScale().fitContent();
          chart?.timeScale().applyOptions({ rightOffset: RIGHT_OFFSET_BARS });
        } else if (savedTimeRange) {
          try {
            chart?.timeScale().setVisibleRange(savedTimeRange);
          } catch {
            if (savedLogicalRange) chart?.timeScale().setVisibleLogicalRange(savedLogicalRange);
          }
        } else if (savedLogicalRange) {
          chart?.timeScale().setVisibleLogicalRange(savedLogicalRange);
        }
        loadedHistoryTargetRef.current = targetKey;
        const range = chart?.timeScale().getVisibleRange();
        overlayRef.current?.setVisibleTimeRange(range ? Number(range.from) : undefined, range ? Number(range.to) : undefined);
      } catch (e) {
        console.error('get_history failed', e);
      }
      useChartStore.getState().setLoadStatus('loading_indicators');
      try {
        const list = await invoke<IctStructure[]>('list_structures', { symbol, tf, watchlistId });
        if (cancelled) return;
        // Defer applySnapshot to the next animation frame so the chart's
        // timeScale has finished laying out the freshly-loaded history
        // before primitives ask `timeToCoordinate(...)` for their
        // anchors. Without this RAF the first call returns null for
        // ts that fall inside the visible window but pre-fitContent,
        // and the rectangles silently fail to draw on TF switch (Bug A).
        requestAnimationFrame(() => {
          if (cancelled) return;
          overlayRef.current?.applySnapshot(list);
          useChartStore.getState().setLoadStatus('ready');
        });
      } catch (e) {
        console.warn('list_structures failed', e);
        useChartStore.getState().setLoadStatus('ready');
      }
    }
    load();
    return () => { cancelled = true; };
  }, [symbol, tf, barsReloadNonce, watchlistId]);

  useEffect(() => {
    let cancelled = false;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    async function reloadStructures() {
      try {
        const list = await invoke<IctStructure[]>('list_structures', { symbol, tf, watchlistId });
        if (cancelled) return;
        requestAnimationFrame(() => {
          if (cancelled) return;
          overlayRef.current?.applySnapshot(list);
        });
        if (list.length === 0 && !cancelled) {
          retryTimer = setTimeout(() => {
            if (!cancelled) void reloadStructures();
          }, 5000);
        }
      } catch (e) {
        console.warn('reload list_structures failed', e);
      }
    }
    void reloadStructures();
    return () => {
      cancelled = true;
      if (retryTimer) clearTimeout(retryTimer);
    };
  }, [structuresReloadNonce, symbol, tf, watchlistId]);

  // Bar event subscription.
  useEffect(() => {
    const unlisteners: UnlistenFn[] = [];
    let mounted = true;
    async function sub() {
      const handler = (closed: boolean) => (e: { payload: BarPayload }) => {
        const b = e.payload;
        if (!mounted) return;
        if (b.symbol !== symbol || b.tf !== tf) return;
        if (!seriesRef.current) return;
        const candle = toCandle(b);
        const tsec = Number(candle.time);
        if (tsec < lastTsRef.current) {
          if (closed && chartRef.current) {
            applyDelayedClose(seriesRef.current, chartRef.current, candle);
            overlayRef.current?.addBarTime(b.ts, b.high, b.low);
          }
          return;
        }
        seriesRef.current.update(candle);
        lastTsRef.current = tsec;
        overlayRef.current?.addBarTime(b.ts, b.high, b.low);
        overlayRef.current?.setLastBarTs(b.ts);
        if (closed) {
          applyBar(b);
        }
      };
      const u1 = await listen<BarPayload>('bar:update', handler(false));
      const u2 = await listen<BarPayload>('bar:closed', handler(true));
      unlisteners.push(u1, u2);
    }
    sub();
    return () => {
      mounted = false;
      unlisteners.forEach((u) => u());
    };
  }, [symbol, tf, applyBar]);

  // ICT structure event subscription.
  useEffect(() => {
    const unlisteners: UnlistenFn[] = [];
    let mounted = true;
    async function sub() {
      const apply = (op: 'new' | 'update' | 'invalidated') => (e: { payload: unknown }) => {
        if (!mounted || !overlayRef.current) return;
        const ev: StructureEvent = { ...(e.payload as object), op } as StructureEvent;
        overlayRef.current.applyEvent(ev);
      };
      unlisteners.push(await listen('ict:structure:new', apply('new')));
      unlisteners.push(await listen('ict:structure:update', apply('update')));
      unlisteners.push(await listen('ict:structure:invalidated', apply('invalidated')));
    }
    sub();
    return () => {
      mounted = false;
      unlisteners.forEach((u) => u());
    };
  }, []);

  return (
    <div ref={containerRef} className="relative w-full h-full" style={{ background: 'var(--bg-0)' }} />
  );
}
