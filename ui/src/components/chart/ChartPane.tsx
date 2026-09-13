import { resolveAlertSmt } from "./alertNavigation";
import { useEffect, useRef, useState } from 'react';
import { PositionLayer, type PositionChart } from './positions/PositionLayer';
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
import type { IctStructure, SmtDivergence, StructureEvent } from '../../types/structures';
import type { CandidateSetup } from '../../types/candidate';
import type { AlertRecord } from '../../types/alert';
import { StructureOverlay, DEFAULT_FILTER } from './overlay/StructureOverlay';
import { RectanglePrimitive, type RectSpec } from './primitives/RectanglePrimitive';
import { chartSync } from './chartSync';

function cssVar(name: string, fallback: string) {
  if (typeof window === 'undefined') return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

function mtfDurationMs(tf: string): number {
  switch (tf) {
    case '30m': return 30 * 60 * 1000;
    case '1h': return 60 * 60 * 1000;
    case '4h': return 4 * 60 * 60 * 1000;
    default: return 60 * 60 * 1000;
  }
}

function toCandle(b: BarPayload) {
  return {
    time: Math.floor(b.ts / 1000) as Time,
    open: b.open, high: b.high, low: b.low, close: b.close,
  };
}

const RIGHT_OFFSET_BARS = 8;
// In three-pane mode each chart is only about 250-300px wide. Showing 120 MTF
// candles made every body roughly 2px wide and hid the sweep structure. Sixty
// keeps useful context on both sides while restoring a readable 4-5px density.
const SMT_NAV_VISIBLE_BARS = 60;

function priceFormatForSymbol(symbol: string) {
  if (symbol.includes('BTC')) return { type: 'price' as const, precision: 2, minMove: 0.01 };
  if (symbol.includes('XAU') || symbol.includes('XAG')) return { type: 'price' as const, precision: 2, minMove: 0.01 };
  if (symbol.includes('DXY')) return { type: 'price' as const, precision: 3, minMove: 0.001 };
  return { type: 'price' as const, precision: 5, minMove: 0.00001 };
}

function dateFromLwcTime(time: Time) {
  if (typeof time === 'number') return new Date(time * 1000);
  if (typeof time === 'string') return new Date(time);
  return new Date(Date.UTC(time.year, time.month - 1, time.day));
}

// ICT concepts (PDH/PDL, sessions, kill zones) are all NY-time based.
// Display the axis in Beijing time (Asia/Shanghai).
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

type ChartPaneProps = {
  symbol: string;
  paneIndex: number;
  historyRows: BarPayload[] | null;
};

export function ChartPane({ symbol, paneIndex, historyRows }: ChartPaneProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [positionChart, setPositionChart] = useState<PositionChart | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<'Candlestick'> | null>(null);
  const overlayRef = useRef<StructureOverlay | null>(null);
  const highlightPrimRef = useRef<RectanglePrimitive | null>(null);
  const candC2HighlightPrimRef = useRef<RectanglePrimitive | null>(null);
  const candC3HighlightPrimRef = useRef<RectanglePrimitive | null>(null);
  const alertC2HighlightPrimRef = useRef<RectanglePrimitive | null>(null);
  const alertC3HighlightPrimRef = useRef<RectanglePrimitive | null>(null);
 const lastTsRef = useRef<number>(0);
  const loadedHistoryTargetRef = useRef<string | null>(null);
  const isSyncingRef = useRef(false);
  const isOriginatorRef = useRef(false);
  const lastSyncApplyRef = useRef(0);

  const tf = useChartStore((s) => s.tf);
  const watchlistId = useChartStore((s) => s.activeWatchlist?.id);
  const applyBar = useChartStore((s) => s.applyBar);
  const structuresReloadNonce = useChartStore((s) => s.structuresReloadNonce);
  const barsReloadNonce = useChartStore((s) => s.barsReloadNonce);
  const smtEnabled = useChartStore((s) => s.smtEnabled);
  const smtChainEnabled = useChartStore((s) => s.smtChainEnabled);
  const smtHtfPdaEnabled = useChartStore((s) => s.smtHtfPdaEnabled);
  const smtSweepLineEnabled = useChartStore((s) => s.smtSweepLineEnabled);
  const highlightSmtId = useChartStore((s) => s.highlightSmtId);
  const highlightCandidateId = useChartStore((s) => s.highlightCandidateId);
  const candidateEnabled = useChartStore((s) => s.candidateEnabled);
  const highlightAlertId = useChartStore((s) => s.highlightAlertId);
  const filters = useDetectorFilters();

  // Create chart once.
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
        locale: 'en-US',
        timeFormatter: (time: Time) => crosshairTimeFormatter.format(dateFromLwcTime(time)),
      },
      grid: {
        vertLines: { color: cssVar('--border', '#2a2e33'), style: 1 },
        horzLines: { color: cssVar('--border', '#2a2e33'), style: 1 },
      },
      handleScale: { axisPressedMouseMove: { time: true, price: true } },
      rightPriceScale: { borderColor: cssVar('--border', '#2a2e33'), autoScale: true },
      timeScale: {
        borderColor: cssVar('--border', '#2a2e33'),
        timeVisible: true, secondsVisible: false,
        rightOffset: RIGHT_OFFSET_BARS, tickMarkFormatter,
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
      lastValueVisible: true, priceLineVisible: true,
      priceLineSource: PriceLineSource.LastBar,
      priceLineStyle: LineStyle.Dashed, priceLineWidth: 1,
      priceLineColor: cssVar('--accent', '#4f8cff'),
    });
    chartRef.current = chart;
    seriesRef.current = series;
    setPositionChart({ chart, series });
    overlayRef.current = new StructureOverlay(chart, series, el, symbol, tf);
    overlayRef.current.setFilter({ ...DEFAULT_FILTER, ...filters });

    // Sync: propagate visible range changes to other panes.
    // RAF-throttled + timestamp-guarded to prevent feedback loops (lwc fires
    // range-change events async via requestAnimationFrame, so a naive guard
    // reset before the event fires -- causing panes to re-broadcast to each
    // other in an infinite loop that freezes the UI).
    let rangeSyncRaf = 0;
    const onRangeChange = () => {
      if (isSyncingRef.current) return;
      if (Date.now() - lastSyncApplyRef.current < 250) return;
      if (rangeSyncRaf) return;
      rangeSyncRaf = requestAnimationFrame(() => {
        rangeSyncRaf = 0;
        const ts = chart.timeScale();
        const tr = ts.getVisibleRange();
        if (!tr) return;
        const from = Number(tr.from);
        const to = Number(tr.to);
        isOriginatorRef.current = true;
        const lr = ts.getVisibleLogicalRange();
        const w = ts.width();
        const barSpacing = lr ? w / (lr.to - lr.from + 1) : 0;
        chartSync.setRange({ from, to, barSpacing });
      });
    };
    chart.timeScale().subscribeVisibleLogicalRangeChange(onRangeChange);

    // Sync: propagate crosshair position to other panes.
   const onCrosshairMove = (param: { time?: Time }) => {
      if (isSyncingRef.current) return;
      if (Date.now() - lastSyncApplyRef.current < 250) return;
      const t = param.time != null ? Number(param.time) : null;
      isOriginatorRef.current = true;
      chartSync.setCrosshair(t);
    };
    chart.subscribeCrosshairMove(onCrosshairMove);

    // Click anywhere on the chart dismisses only the yellow navigation
    // rectangles, including the selected SMT's MTF PDA overlay. The selected
    // SMT evidence (especially the white sweep line) and persistent purple
    // PDA stay pinned for inspection until another inbox context is selected.
    const onChartClick = () => {
      useChartStore.getState().setHighlightSmt(null);
      useChartStore.getState().setHighlightCandidate(null);
      useChartStore.getState().setHighlightAlert(null);
    };
    chart.subscribeClick(onChartClick);

    // Sync: apply incoming time range from other panes (independent channel
    // from crosshair to avoid cross-firing a scroll re-applying a stale
    // crosshair or a mouse-move re-broadcasting a stale range).
    const unsubRange = chartSync.subscribeRange((r) => {
      if (!r) return;
      if (isOriginatorRef.current) {
        isOriginatorRef.current = false;
        return;
      }
     lastSyncApplyRef.current = Date.now();
      isSyncingRef.current = true;
      try {
        const ts = chart.timeScale();
        // Preserve the originator's zoom: applyOptions({ barSpacing }) sets
        // ONLY the bar spacing (ApplyBarSpacing), and scrollToPosition sets
        // ONLY the right offset (ApplyRightOffset) -- neither recalculates
        // the other, unlike setVisibleRange which always recomputes bar
        // spacing = width / range length.
        if (r.barSpacing > 0) {
          const lr = ts.getVisibleLogicalRange();
          const scroll = ts.scrollPosition();
          if (lr) {
            const baseIndex = lr.to - scroll;
            const w = ts.width();
            const fromCoord = ts.timeToCoordinate(r.from as Time);
            const logicalFrom = fromCoord !== null ? ts.coordinateToLogical(fromCoord) : null;
            if (logicalFrom !== null) {
              const newVisibleBars = w / r.barSpacing;
              const targetRightOffset = logicalFrom + newVisibleBars - 1 - baseIndex;
              ts.applyOptions({ barSpacing: r.barSpacing });
              ts.scrollToPosition(targetRightOffset, false);
            } else {
              ts.setVisibleRange({ from: r.from as Time, to: r.to as Time });
            }
          } else {
            ts.setVisibleRange({ from: r.from as Time, to: r.to as Time });
          }
        } else {
          ts.setVisibleRange({ from: r.from as Time, to: r.to as Time });
        }
      } catch { /* times may not exist on this symbol yet */ }
      isSyncingRef.current = false;
    });

    // Sync: apply incoming crosshair from other panes.
    const unsubCrosshair = chartSync.subscribeCrosshair((t) => {
      if (isOriginatorRef.current) {
        isOriginatorRef.current = false;
        return;
      }
      lastSyncApplyRef.current = Date.now();
      if (t !== null) {
        isSyncingRef.current = true;
        try {
          chart.setCrosshairPosition(0, t as Time, series);
        } catch { /* time may not exist on this symbol */ }
        isSyncingRef.current = false;
      } else {
        chart.clearCrosshairPosition();
      }
    });

    const syncVisibleRange = () => {
      const range = chart.timeScale().getVisibleRange();
      overlayRef.current?.setVisibleTimeRange(range ? Number(range.from) : undefined, range ? Number(range.to) : undefined);
    };
    chart.timeScale().subscribeVisibleTimeRangeChange(syncVisibleRange);
    chart.timeScale().subscribeVisibleLogicalRangeChange(syncVisibleRange);

    if (paneIndex === 0) {
      (window as unknown as { __ovl?: unknown }).__ovl = overlayRef.current;
    }

  return () => {
      if (rangeSyncRaf) cancelAnimationFrame(rangeSyncRaf);
      unsubRange();
      unsubCrosshair();
      chart.timeScale().unsubscribeVisibleLogicalRangeChange(onRangeChange);
      chart.timeScale().unsubscribeVisibleTimeRangeChange(syncVisibleRange);
      chart.timeScale().unsubscribeVisibleLogicalRangeChange(syncVisibleRange);
      chart.unsubscribeCrosshairMove(onCrosshairMove);
      chart.unsubscribeClick(onChartClick);
      overlayRef.current?.dispose();
      overlayRef.current = null;
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const ts = chartRef.current?.timeScale();
    const savedLogical = ts?.getVisibleLogicalRange();
    overlayRef.current?.setFilter(filters);
    if (savedLogical) {
      ts?.setVisibleLogicalRange(savedLogical);
    }
  }, [filters]);

  useEffect(() => {
    overlayRef.current?.setSmtEnabled?.({
      enabled: smtEnabled,
      chain: smtChainEnabled,
      htfPda: smtHtfPdaEnabled,
      sweepLine: smtSweepLineEnabled,
    });
  }, [smtEnabled, smtChainEnabled, smtHtfPdaEnabled, smtSweepLineEnabled]);

  useEffect(() => {
    overlayRef.current?.setCandidateEnabled?.(candidateEnabled);
  }, [candidateEnabled]);

  // SMT inbox click -> highlight the chain region on the chart with a
  // full-height band. Managed directly on the series so it doesn't
  // depend on the StructureOverlay instance (which is created once on
  // mount and not refreshed by HMR).
  useEffect(() => {
    const detachBand = () => {
      if (highlightPrimRef.current) {
        try { seriesRef.current?.detachPrimitive(highlightPrimRef.current); } catch { /* ignore */ }
        highlightPrimRef.current = null;
      }
      overlayRef.current?.setHighlightedSmt(null);
    };
    if (!highlightSmtId) { detachBand(); return; }
    // MultiPaneContainer owns the one shared, calendar-cropped history
    // snapshot used by all open panes. Wait for that exact snapshot before
    // calculating the selection rectangle; a private get_history response
    // has a different logical origin whenever one symbol has more leading
    // bars and makes only that pane's yellow box drift horizontally.
    if (!historyRows) { detachBand(); return; }
    let cancelled = false;
    const scrollTimers: ReturnType<typeof setTimeout>[] = [];
    void (async () => {
      console.debug('[smt-nav] opening', { id: highlightSmtId, symbol, tf });
      let smt: SmtDivergence | undefined;
      let lastLoadError: unknown = null;
      // BottomBar can finish loading before the chart series has mounted on
      // a cold app start. Previously that made the first row click update the
      // table selection but silently skip all chart navigation. Retry for a
      // short bounded window so the first click remains authoritative.
      for (let attempt = 0; attempt < 12 && !cancelled; attempt += 1) {
        try {
          const list = await invoke<SmtDivergence[]>('list_smt', { watchlistId });
          smt = list.find((item) => item.id === highlightSmtId);
          lastLoadError = null;
        } catch (error) {
          lastLoadError = error;
        }
        if (seriesRef.current && smt && tf === smt.comparison_timeframe) break;
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
      if (cancelled) return;
      if (lastLoadError) {
        console.warn('[smt-nav] list_smt failed', lastLoadError);
        return;
      }
      if (!smt || tf !== smt.comparison_timeframe) {
        console.warn('[smt-nav] row/TF mismatch', { id: highlightSmtId, symbol, tf, foundTf: smt?.comparison_timeframe });
        detachBand();
        return;
      }
      if (!seriesRef.current) {
        console.warn('[smt-nav] chart series did not mount before navigation timeout', { id: highlightSmtId, symbol, tf });
        return;
      }
      const symbolChain = smt.chains.find((c) => c.symbol === symbol);
      // Each pane owns an independent C1/SMT-K/C2/C3 chain. Borrowing DXY's
      // chain for EU/GU produces a plausible-looking rectangle on the wrong
      // candles and price scale.
      const chain = symbolChain;
      if (!chain) { detachBand(); return; }
      overlayRef.current?.setHighlightedSmt(smt);
      const startSec = Math.floor(chain.c1_candle.ts / 1000);
      const finalChainBarTs = chain.c3_candle?.ts ?? chain.c2_candle?.ts ?? chain.smt_k_candle.ts;
      // The audit box covers complete MTF candles. Using the C3 open as the
      // right edge made C3 look excluded from the selected chain.
      const endSec = Math.floor((finalChainBarTs + mtfDurationMs(smt.comparison_timeframe)) / 1000);
      if (!(endSec > startSec)) { detachBand(); return; }
      const paneBars = historyRows;
      const tfMs = mtfDurationMs(smt.comparison_timeframe);
      const actualChainBars = paneBars.filter((bar) => {
        return bar.ts >= chain.c1_candle.ts && bar.ts <= finalChainBarTs;
      });
      const expectedBarCount = Math.floor(
        (finalChainBarTs - chain.c1_candle.ts) / tfMs,
      ) + 1;
      const hasCompleteCanonicalSpan = actualChainBars.length === expectedBarCount
        && actualChainBars.every(
          (bar, index) => bar.ts === chain.c1_candle.ts + index * tfMs,
        );
      // Visible SMT rows are required to be reproducible on every pane. If
      // canonical history cannot resolve an endpoint, hide the selection and
      // let the backend chain audit invalidate the row; never turn a C1-C3
      // label into an SMT-K-C3 rectangle.
      if (!hasCompleteCanonicalSpan) { detachBand(); return; }
      const rectStartSec = startSec;
      const rectEndSec = endSec;
      const rectLastBarSec = Math.floor(finalChainBarTs / 1000);
      const lows: number[] = [];
      const highs: number[] = [];
      for (const bar of actualChainBars) {
        lows.push(bar.low);
        highs.push(bar.high);
      }
      if (lows.length === 0 || highs.length === 0) { detachBand(); return; }
      let priceLow = Math.min(...lows);
      let priceHigh = Math.max(...highs);
      const priceSpan = priceHigh - priceLow;
      const pricePad = priceSpan > 0 ? priceSpan * 0.3 : Math.max(Math.abs(priceHigh) * 0.002, 0.0001);
      priceLow -= pricePad;
      priceHigh += pricePad;
      // Bind the audit rectangle to real logical bars as well as timestamps.
      // During synchronized zoom lightweight-charts can transiently return
      // null from timeToCoordinate even for a visible timestamp; the generic
      // rectangle fallback then clamps that endpoint to the pane edge and
      // turns a two-bar C1/SMT-K box into a long horizontal band.
      const paneTimelineMs = Array.from(new Set(paneBars.map((bar) => bar.ts))).sort((a, b) => a - b);
      const fallbackStartLogical = paneTimelineMs.indexOf(rectStartSec * 1000);
      const fallbackLastLogical = paneTimelineMs.indexOf(rectLastBarSec * 1000);
      // Do not consult StructureOverlay here. This effect is declared before
      // the history-loading effect, so after a TF switch its timestamp map can
      // still belong to the previous series for one render. The indices from
      // `historyRows` are the authoritative indices of series.setData below.
      const startLogical = fallbackStartLogical >= 0 ? fallbackStartLogical : undefined;
      const lastLogical = fallbackLastLogical >= 0 ? fallbackLastLogical : undefined;
      const hasStableLogicalSpan = typeof startLogical === 'number'
        && typeof lastLogical === 'number'
        && lastLogical >= startLogical;
      const auditLabel = chain.c3_candle
        ? chain.detection_state === 'invalidated' ? 'C1–C3失败' : 'C1–C3'
        : chain.c2_candle ? 'C1–C2' : 'C1–SMT K';
      const spec: RectSpec = {
        startTime: rectStartSec as Time,
        endTime: rectEndSec as Time,
        startLogical: hasStableLogicalSpan ? startLogical : undefined,
        endLogical: hasStableLogicalSpan ? lastLogical + 1 : undefined,
        priceLow,
        priceHigh,
        zOrder: 'top',
        style: { fill: 'rgba(255,235,59,0.18)', stroke: 'rgba(255,235,59,0.85)', strokeWidth: 2, visible: true },
        label: auditLabel,
        forceLabel: true,
      };
      if (highlightPrimRef.current) {
        highlightPrimRef.current.setSpec(spec);
      } else {
        const prim = new RectanglePrimitive(spec);
        try { seriesRef.current.attachPrimitive(prim); } catch { return; }
        highlightPrimRef.current = prim;
      }
      // Locate the band at a normal browsing density. The old range used the
      // 2-3 candle chain itself as the zoom span, making each candle enormous.
      // Keep the whole chain visible, but never show fewer than about 60 MTF
      // candles. Deferred retries are still required because the async history
      // load calls fitContent() while a row click may also switch timeframe.
      const tfSec = Math.max(1, mtfDurationMs(smt.comparison_timeframe) / 1000);
      const centerSec = (startSec + endSec) / 2;
      const halfSpanSec = Math.max(
        tfSec * SMT_NAV_VISIBLE_BARS / 2,
        (endSec - startSec) / 2 + tfSec * 4,
      );
      const range = {
        from: Math.floor(centerSec - halfSpanSec) as Time,
        to: Math.ceil(centerSec + halfSpanSec) as Time,
      };
      const doScroll = () => {
        if (cancelled) return;
        const chart = chartRef.current;
        if (!chart) return;
        try { chart.timeScale().setVisibleRange(range); } catch { /* ignore */ }
      };
      console.debug('[smt-nav] ready', { id: smt.id, symbol, tf, hasSymbolChain: Boolean(symbolChain), startSec, endSec });
      doScroll();
      scrollTimers.push(setTimeout(doScroll, 350));
      scrollTimers.push(setTimeout(doScroll, 800));
      scrollTimers.push(setTimeout(doScroll, 1500));
    })();
    return () => { cancelled = true; for (const t of scrollTimers) clearTimeout(t); };
  }, [highlightSmtId, symbol, tf, historyRows, watchlistId]);

  // Candidate highlight bands (M6a): split the validation window into
  // individually labelled C2 (cyan) and C3 (blue-violet) MTF periods.
  useEffect(() => {
   const detachBand = (suppressReversals = false) => {
     for (const ref of [candC2HighlightPrimRef, candC3HighlightPrimRef]) {
       if (!ref.current) continue;
       try { seriesRef.current?.detachPrimitive(ref.current); } catch { /* ignore */ }
       ref.current = null;
     }
      overlayRef.current?.setHighlightRange(null, null, suppressReversals);
   };
   if (!highlightCandidateId) { detachBand(); return; }
    overlayRef.current?.clearPinnedSmt();
    let cancelled = false;
    const scrollTimers: ReturnType<typeof setTimeout>[] = [];
    void (async () => {
      let cand: CandidateSetup | undefined;
      let smts: SmtDivergence[] = [];
      let lastLoadError: unknown = null;
      // A Candidate row changes the global timeframe before setting the
      // highlight id. On a cold chart (or a rapid TF change) the request can
      // finish while the candlestick series is still being rebuilt. Retry the
      // same bounded window used by SMT navigation instead of silently losing
      // the first click.
      for (let attempt = 0; attempt < 12 && !cancelled; attempt += 1) {
        try {
          const [list, loadedSmts] = await Promise.all([
            invoke<CandidateSetup[]>('list_candidates', { watchlistId }),
            invoke<SmtDivergence[]>('list_smt', { watchlistId }),
          ]);
          cand = list.find((item) => item.id === highlightCandidateId);
          smts = loadedSmts;
          lastLoadError = null;
        } catch (error) {
          lastLoadError = error;
        }
        // Candidate rectangles belong only on the validation/LTF chart. If
        // the user deliberately switches to an MTF/HTF chart, remove the
        // navigation aid immediately, stop forcing LTF reversal labels, and
        // never overwrite that chart's range.
        if (cand && tf !== cand.validation_timeframe) {
          useChartStore.getState().setHighlightForceIndicators(false);
          detachBand();
          return;
        }
        if (seriesRef.current && cand) break;
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
      if (cancelled) return;
      if (lastLoadError) {
        useChartStore.getState().setHighlightForceIndicators(false);
        console.warn('[candidate-nav] list_candidates/list_smt failed', lastLoadError);
        return;
      }
      if (!cand) {
        useChartStore.getState().setHighlightForceIndicators(false);
        console.warn('[candidate-nav] candidate not found', { id: highlightCandidateId, symbol, tf });
        detachBand();
        return;
      }
      if (tf !== cand.validation_timeframe) {
        useChartStore.getState().setHighlightForceIndicators(false);
        detachBand();
        return;
      }
      if (!seriesRef.current) {
        useChartStore.getState().setHighlightForceIndicators(false);
        console.warn('[candidate-nav] chart series did not mount before navigation timeout', { id: highlightCandidateId, symbol, tf });
        return;
      }
      useChartStore.getState().setHighlightForceIndicators(true);
    const sourceSmt = smts.find((item) => item.id === cand.smt_id);
    if (!sourceSmt) {
      useChartStore.getState().setHighlightForceIndicators(false);
      console.warn('[candidate-nav] source SMT snapshot unavailable', {
        candidateId: cand.id,
        smtId: cand.smt_id,
        symbol,
      });
      detachBand();
      return;
    }
    const symbolChain = sourceSmt?.chains.find((item) => item.symbol === symbol);
    // v9 chains are independent per symbol. A DXY Candidate can exist while
    // EU or GU has not yet formed its own C2, so do not project DXY's window
    // onto that pane. A missing source snapshot is handled above rather than
    // borrowing the Candidate's global DXY window.
    const localC2 = symbolChain?.c2_candle;
    const localC3 = symbolChain?.c3_candle;
    if (!localC2) {
      // This pane participates in the selected SMT but has no executable C2
      // window of its own. Keep it in focused-audit mode with an empty
      // reversal window so the global force-indicator flag cannot expose all
      // historical MSS/CISD markers on this symbol.
      detachBand(true);
      return;
    }
    // Candidate consideration window: C2 open through the end of the C3
    // MTF candle. Render separate bands so the LTF pane shows which MTF
    // confirmation period each CISD/MSS event belongs to.
    const mtfDur = mtfDurationMs(cand.comparison_timeframe);
    const windowEndMs = (localC3?.ts ?? localC2.ts) + mtfDur;
    overlayRef.current?.setHighlightRange(
      { start: localC2.ts, end: windowEndMs },
      ((cand.validations ?? []).find((validation) => validation.symbol === symbol)
        ?? (cand.validation_symbol === symbol && cand.validation_ts && cand.validation_kind && cand.validation_direction
        ? {
            symbol: cand.validation_symbol,
            ts: cand.validation_ts,
            kind: cand.validation_kind,
            direction: cand.validation_direction,
            event_id: '',
            price: 0,
          }
        : null))
        ? (() => {
            const validation = (cand.validations ?? []).find((item) => item.symbol === symbol)
              ?? {
                symbol: cand.validation_symbol!,
                ts: cand.validation_ts!,
                kind: cand.validation_kind!,
                direction: cand.validation_direction!,
              };
            return { ...validation, label: 'VALIDATED' };
          })()
        : null,
    );
     const startSec = Math.floor(localC2.ts / 1000);
      const c2EndSec = Math.floor((localC2.ts + mtfDur) / 1000);
      const c3StartSec = localC3 ? Math.floor(localC3.ts / 1000) : null;
      const endSec = Math.floor(windowEndMs / 1000);
      if (!(endSec >= startSec)) { detachBand(); return; }
      const c2Spec: RectSpec = {
        startTime: startSec as Time,
        endTime: c2EndSec as Time,
        priceLow: 0,
        priceHigh: 1,
        fullHeight: true,
        zOrder: 'top',
        label: 'C2',
        forceLabel: true,
        style: { fill: 'rgba(0,210,255,0.16)', stroke: 'rgba(0,220,255,0.9)', strokeWidth: 2, visible: true },
      };
      if (candC2HighlightPrimRef.current) {
        candC2HighlightPrimRef.current.setSpec(c2Spec);
      } else {
        const prim = new RectanglePrimitive(c2Spec);
        try { seriesRef.current.attachPrimitive(prim); } catch { return; }
        candC2HighlightPrimRef.current = prim;
      }
      if (c3StartSec !== null) {
        const c3Spec: RectSpec = {
          startTime: c3StartSec as Time,
          endTime: endSec as Time,
          priceLow: 0,
          priceHigh: 1,
          fullHeight: true,
          zOrder: 'top',
          label: 'C3',
          forceLabel: true,
          style: { fill: 'rgba(92,110,255,0.18)', stroke: 'rgba(130,145,255,0.95)', strokeWidth: 2, visible: true },
        };
        if (candC3HighlightPrimRef.current) {
          candC3HighlightPrimRef.current.setSpec(c3Spec);
        } else {
          const prim = new RectanglePrimitive(c3Spec);
          try { seriesRef.current.attachPrimitive(prim); } catch { return; }
          candC3HighlightPrimRef.current = prim;
        }
      } else if (candC3HighlightPrimRef.current) {
        try { seriesRef.current.detachPrimitive(candC3HighlightPrimRef.current); } catch { /* ignore */ }
        candC3HighlightPrimRef.current = null;
      }
      const pad = Math.max((endSec - startSec), 7200);
      const range = { from: (startSec - pad) as Time, to: (endSec + pad) as Time };
      const doScroll = () => {
        if (cancelled) return;
        const chart = chartRef.current;
        if (!chart) return;
        try { chart.timeScale().setVisibleRange(range); } catch { /* ignore */ }
      };
      doScroll();
      scrollTimers.push(setTimeout(doScroll, 350));
      scrollTimers.push(setTimeout(doScroll, 800));
    })();
    return () => { cancelled = true; for (const t of scrollTimers) clearTimeout(t); };
  }, [highlightCandidateId, symbol, tf, watchlistId]);

  // Keep the candidate highlight until the user clicks the chart or chooses
  // another row. It is a navigation aid, not a transient notification.
  useEffect(() => {
    if (!highlightCandidateId) return;
    useChartStore.getState().setHighlightForceIndicators(true);
    return () => useChartStore.getState().setHighlightForceIndicators(false);
  }, [highlightCandidateId]);

  // Alert highlight bands (M6b): split C2 (amber) and C3 (red-orange),
  // spanning the full visible price height.
  // Price range = full chart height (avoid symbol price mismatch).
  useEffect(() => {
   const detachBand = (suppressReversals = false) => {
     for (const ref of [alertC2HighlightPrimRef, alertC3HighlightPrimRef]) {
       if (!ref.current) continue;
       try { seriesRef.current?.detachPrimitive(ref.current); } catch { /* ignore */ }
       ref.current = null;
     }
      overlayRef.current?.setHighlightRange(null, null, suppressReversals);
   };
   if (!highlightAlertId) { detachBand(); return; }
    overlayRef.current?.clearPinnedSmt();
    let cancelled = false;
    const scrollTimers: ReturnType<typeof setTimeout>[] = [];
    void (async () => {
      let alert: AlertRecord | undefined;
      let smts: SmtDivergence[] = [];
      let lastLoadError: unknown = null;
      // Alert navigation has the same cold-mount race as Candidate
      // navigation. Keep retrying briefly until both the record and series
      // are ready, but never project an LTF alert range onto another TF.
      for (let attempt = 0; attempt < 12 && !cancelled; attempt += 1) {
        try {
          const [alerts, reversals, loadedSmts] = await Promise.all([
            invoke<AlertRecord[]>('list_alerts', { limit: null, watchlistId }),
            invoke<AlertRecord[]>('list_reversals', { limit: null, watchlistId }),
            invoke<SmtDivergence[]>('list_smt', { watchlistId }),
          ]);
          alert = [...alerts, ...reversals].find((item) => item.id === highlightAlertId);
          smts = loadedSmts;
          lastLoadError = null;
        } catch (error) {
          lastLoadError = error;
        }
        if (alert && tf !== alert.validation_timeframe) {
          useChartStore.getState().setHighlightForceIndicators(false);
          detachBand();
          return;
        }
        if (seriesRef.current && alert) break;
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
      if (cancelled) return;
      if (lastLoadError) {
        useChartStore.getState().setHighlightForceIndicators(false);
        console.warn('[alert-nav] inbox records/list_smt failed', lastLoadError);
        return;
      }
      if (!alert) {
        useChartStore.getState().setHighlightForceIndicators(false);
        console.warn('[alert-nav] alert not found', { id: highlightAlertId, symbol, tf });
        detachBand();
        return;
      }
      if (tf !== alert.validation_timeframe) {
        useChartStore.getState().setHighlightForceIndicators(false);
        detachBand();
        return;
      }
      if (!seriesRef.current) {
        useChartStore.getState().setHighlightForceIndicators(false);
        console.warn('[alert-nav] chart series did not mount before navigation timeout', { id: highlightAlertId, symbol, tf });
        return;
      }
      useChartStore.getState().setHighlightForceIndicators(true);
    let sourceSmt: SmtDivergence | null;
    try {
      sourceSmt = await resolveAlertSmt(alert.smt_id, smts, (smtId) =>
        invoke<SmtDivergence | null>('get_smt_snapshot', { smtId, watchlistId: alert!.watchlist_id }),
      );
    } catch (error) {
      if (cancelled) return;
      console.warn('[alert-nav] historical snapshot lookup failed', error);
      useChartStore.getState().setHighlightForceIndicators(false);
      detachBand();
      return;
    }
    if (cancelled) return;
    if (!sourceSmt) {
      useChartStore.getState().setHighlightForceIndicators(false);
      console.warn('[alert-nav] source SMT snapshot unavailable', {
        alertId: alert.id,
        smtId: alert.smt_id,
        symbol,
      });
      detachBand();
      return;
    }
    const symbolChain = sourceSmt?.chains.find((item) => item.symbol === symbol);
    // The clicked symbol uses the immutable fired snapshot, even if its
    // current chain lost C2 after invalidation/replay correction.
    const localC2Ts = alert.validation_symbol === symbol
      ? alert.c2_candle_ts : symbolChain?.c2_candle?.ts;
    const localC3Ts = alert.validation_symbol === symbol
      ? alert.c3_candle_ts : symbolChain?.c3_candle?.ts;
    if (localC2Ts === undefined) {
      // A per-symbol Alert Inbox row does not imply that every other trade
      // symbol confirmed C2. On panes without a local C2/C3 window, focused
      // navigation must show zero reversal markers rather than every marker
      // made visible by the shared force-indicator state.
      detachBand(true);
      return;
    }
    // Alert consideration window matches candidate validation exactly:
    // C2 open through the end of the C3 MTF candle.
    const mtfDur = mtfDurationMs(alert.comparison_timeframe);
    const windowEndMs = (localC3Ts ?? localC2Ts) + mtfDur;
    overlayRef.current?.setHighlightRange(
      { start: localC2Ts, end: windowEndMs },
      alert.validation_symbol && alert.validation_ts && alert.validation_kind && alert.validation_direction
        ? {
            symbol: alert.validation_symbol,
            ts: alert.validation_ts,
            kind: alert.validation_kind as 'cisd' | 'mss',
            direction: alert.validation_direction as 'bullish' | 'bearish',
            label: 'ALERT',
          }
        : null,
    );
     const startSec = Math.floor(localC2Ts / 1000);
      const c2EndSec = Math.floor((localC2Ts + mtfDur) / 1000);
      const c3StartSec = localC3Ts == null
        ? null
        : Math.floor(localC3Ts / 1000);
      const endSec = Math.floor(windowEndMs / 1000);
      const c2Spec: RectSpec = {
        startTime: startSec as Time,
        endTime: c2EndSec as Time,
        priceLow: 0,
        priceHigh: 1,
        fullHeight: true,
        zOrder: 'top',
        label: 'C2',
        forceLabel: true,
        style: { fill: 'rgba(255,175,0,0.17)', stroke: 'rgba(255,190,35,0.95)', strokeWidth: 2, visible: true },
      };
      if (alertC2HighlightPrimRef.current) {
        alertC2HighlightPrimRef.current.setSpec(c2Spec);
      } else {
        const prim = new RectanglePrimitive(c2Spec);
        try { seriesRef.current.attachPrimitive(prim); } catch { return; }
        alertC2HighlightPrimRef.current = prim;
      }
      if (c3StartSec !== null) {
        const c3Spec: RectSpec = {
          startTime: c3StartSec as Time,
          endTime: endSec as Time,
          priceLow: 0,
          priceHigh: 1,
          fullHeight: true,
          zOrder: 'top',
          label: 'C3',
          forceLabel: true,
          style: { fill: 'rgba(255,82,55,0.18)', stroke: 'rgba(255,105,75,0.95)', strokeWidth: 2, visible: true },
        };
        if (alertC3HighlightPrimRef.current) {
          alertC3HighlightPrimRef.current.setSpec(c3Spec);
        } else {
          const prim = new RectanglePrimitive(c3Spec);
          try { seriesRef.current.attachPrimitive(prim); } catch { return; }
          alertC3HighlightPrimRef.current = prim;
        }
      } else if (alertC3HighlightPrimRef.current) {
        try { seriesRef.current.detachPrimitive(alertC3HighlightPrimRef.current); } catch { /* ignore */ }
        alertC3HighlightPrimRef.current = null;
      }
      // Zoom to the band region.
      const pad = Math.max((endSec - startSec), 7200);
      const range = { from: (startSec - pad) as Time, to: (endSec + pad) as Time };
      const doScroll = () => {
        if (cancelled) return;
        const chart = chartRef.current;
        if (!chart) return;
        try { chart.timeScale().setVisibleRange(range); } catch { /* ignore */ }
      };
      doScroll();
      scrollTimers.push(setTimeout(doScroll, 350));
      scrollTimers.push(setTimeout(doScroll, 800));
      scrollTimers.push(setTimeout(doScroll, 1500));
    })();
    return () => { cancelled = true; for (const t of scrollTimers) clearTimeout(t); };
  }, [highlightAlertId, symbol, tf, watchlistId]);

  // Keep the alert highlight until explicit chart interaction/row change.
  useEffect(() => {
    if (!highlightAlertId) return;
    useChartStore.getState().setHighlightForceIndicators(true);
    return () => useChartStore.getState().setHighlightForceIndicators(false);
  }, [highlightAlertId]);


  // Load history + structures when symbol/tf change.
  useEffect(() => {
    let cancelled = false;
    overlayRef.current?.setTarget(symbol, tf);

    async function load() {
      if (cancelled || !seriesRef.current) return;
      const chart = chartRef.current;
      // The viewport belongs to the candle target, not to the watchlist
      // metadata. On startup watchlistId can arrive after the first bars; if
      // it were part of this key that late initialization would still reset
      // a zoom the user had already made.
      const targetKey = `${symbol}\u0000${tf}`;
      if (!historyRows) {
        // Keep the previous complete snapshot visible until the new target's
        // local snapshot is ready. Live events are already guarded by
        // loadedHistoryTargetRef, so retaining it cannot mix timeframes. This
        // avoids the long blank/single-candle flash seen on cold TF switches
        // and Inbox navigation.
        return;
      }
      const isRefresh = loadedHistoryTargetRef.current === targetKey;
      const savedTimeRange = isRefresh ? chart?.timeScale().getVisibleRange() : null;
      const savedLogicalRange = isRefresh ? chart?.timeScale().getVisibleLogicalRange() : null;
      // Internal setData/range changes must not be rebroadcast as a manual
      // multi-pane pan while the other panes are loading the same snapshot.
      if (isRefresh) lastSyncApplyRef.current = Date.now();
      // MultiPaneContainer already de-duplicates, sorts, and crops this exact
      // snapshot to the common calendar start shared by every open pane.
      const data = historyRows.map(toCandle);
      seriesRef.current.setData(data);
      seriesRef.current.applyOptions({ priceFormat: priceFormatForSymbol(symbol) });
      if (!isRefresh) chart?.priceScale('right').applyOptions({ autoScale: true });
      lastTsRef.current = data.length ? Number(data[data.length - 1].time) : 0;
      overlayRef.current?.setBarTimes(data.map((c) => ({ time: Number(c.time), high: c.high, low: c.low })));
      if (historyRows.length) {
        const lastMs = historyRows[historyRows.length - 1].ts;
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
      useChartStore.getState().setLoadStatus('loading_indicators');
      try {
        const list = await invoke<IctStructure[]>('list_structures', { symbol, tf, watchlistId });
        if (cancelled) return;
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
    void load();
    return () => { cancelled = true; };
  }, [symbol, tf, barsReloadNonce, historyRows, watchlistId]);

  useEffect(() => {
   let cancelled = false;
   let retryTimer: ReturnType<typeof setTimeout> | undefined;
   let retryCount = 0;
   async function reloadStructures() {
     try {
       const list = await invoke<IctStructure[]>('list_structures', { symbol, tf, watchlistId });
       if (cancelled) return;
       requestAnimationFrame(() => {
         if (cancelled) return;
         overlayRef.current?.applySnapshot(list);
       });
       // If the engine is still seeding (cold-start), list_structures
       // returns empty via try_lock fallback. Retry with increasing
       // delay until seeding finishes and seed_complete fires.
       if (list.length === 0 && !cancelled) {
         const delay = Math.min(3000 * (retryCount + 1), 20000);
         retryCount++;
         retryTimer = setTimeout(() => {
           if (!cancelled) void reloadStructures();
         }, delay);
       }
     } catch (e) {
       console.warn('reload list_structures failed', e);
     }
   }
   void reloadStructures();
   // Re-fetch when cold-start seed / PO3 replay finishes: during seeding
   // the backend suppresses event broadcasts, so the front-end must
   // re-call list_structures to pick up the full hydrated set.
   const unlisteners: UnlistenFn[] = [];
   (async () => {
     unlisteners.push(await listen('seed_complete', () => { if (!cancelled) { retryCount = 0; void reloadStructures(); } }));
     unlisteners.push(await listen('po3_replay_done', () => { if (!cancelled) void reloadStructures(); }));
   })();
   return () => {
     cancelled = true;
     if (retryTimer) clearTimeout(retryTimer);
     unlisteners.forEach((u) => u());
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
        if (loadedHistoryTargetRef.current !== `${symbol}\u0000${tf}`) return;
        if (!seriesRef.current) return;
        const candle = toCandle(b);
        const tsec = Number(candle.time);
        // lightweight-charts expects repeated updates for the currently-open
        // candle. Only reject genuinely older events; an equal timestamp must
        // update OHLC and the final BarClosed payload for that candle.
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
        if (closed) applyBar(b);
      };
      unlisteners.push(await listen<BarPayload>('bar:update', handler(false)));
      unlisteners.push(await listen<BarPayload>('bar:closed', handler(true)));
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
    <div className="relative w-full h-full" style={{ background: 'var(--bg-0)' }}>
      <div ref={containerRef} className="absolute inset-0" />
      {positionChart && <PositionLayer key={`${symbol}|${tf}`} api={positionChart} symbol={symbol} tf={tf} paneIndex={paneIndex} ready={!!historyRows?.length} />}
    </div>
  );
}
