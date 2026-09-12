// Per-(symbol, tf) overlay manager. Bridges Tauri `ict:structure:*` events
// onto the chart: rectangles for FVG/OB, price lines for PDH/PDL,
// and series markers for MSS/CISD.
//
// The manager owns *all* primitives/markers/lines it creates; `clear()` is
// the only way to fully reset (used when the user switches TF/symbol).

import {
  type IChartApi,
  type ISeriesApi,
  type IPriceLine,
  type SeriesMarker,
  type Time,
  LineStyle,
  createSeriesMarkers,
} from 'lightweight-charts';
import { RectanglePrimitive, type RectSpec } from '../primitives/RectanglePrimitive';
import { SmtSweepPrimitive, type SmtSweepSpec } from '../primitives/SmtSweepPrimitive';
import type {
  Bos,
  BreakerBlock,
  Cisd,
  EqualHighsLows,
  Fvg,
  GapZone,
  IctStructure,
  KillZoneWindow,
  LevelMarker,
  LiquidityReversal,
  LiquiditySweep,
  Mss,
  OrderBlock,
  OteZone,
  PremiumDiscount,
  PowerOf3,
  SessionRange,
  SmtDivergence,
  StructureEvent,
  VolumeImbalance,
} from '../../../types/structures';

type PrimEntry = { id: string; prim: RectanglePrimitive; structure: IctStructure };
type LevelEntry = { line: IPriceLine; level: LevelMarker; autoscalePrim: RectanglePrimitive };
type MarkerStructure = Mss | Cisd | Bos | LiquiditySweep | LiquidityReversal | EqualHighsLows | PowerOf3;
type MarkerGroup = {
  timeSec: number;
  mss?: Mss;
  cisd?: Cisd;
  bos?: Bos;
  sweep?: LiquiditySweep;
  reversal?: LiquidityReversal;
  po3?: PowerOf3;
  eq?: EqualHighsLows;

};
type Po3SignalLine = {
  ts: number;
  price: number;
  tf: string;
  kind: 'cisd' | 'mss';
};

type HighlightConfirm = {
  symbol: string;
  ts: number;
  kind: 'cisd' | 'mss';
  direction: 'bullish' | 'bearish';
  label: 'VALIDATED' | 'ALERT';
};

function cssVar(name: string, fallback: string) {
  if (typeof window === 'undefined') return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

function directionColor(direction: 'bullish' | 'bearish') {
  return direction === 'bullish' ? cssVar('--bull', '#26a69a') : cssVar('--bear', '#ef5350');
}

function structureLineColor() {
  return 'rgba(255, 255, 255, 0.92)';
}

function po3ConfirmLineColor() {
  return 'rgba(100, 181, 246, 0.92)';
}

function triggerBandHalfHeight(price: number) {
  return Math.max(price * 0.000001, 0.000001);
}

const FVG_FILL = {
  bullish: { fill: 'rgba(38, 166, 154, 0.18)', stroke: 'rgba(38, 166, 154, 0.78)' },
  bearish: { fill: 'rgba(239, 83, 80, 0.18)', stroke: 'rgba(239, 83, 80, 0.78)' },
};
// IFVG keeps the FVG's original directional color but draws a thick solid
// border so 'inverted' state reads at a glance. Per user feedback the
// inverted-color rule from §5.2.14 was confusing, so we override here.
const FVG_INV_BORDER_WIDTH = 2.5;
const OB_FILL = {
  bullish: { fill: 'rgba(38, 166, 154, 0.20)', stroke: 'rgba(38, 166, 154, 0.65)' },
  bearish: { fill: 'rgba(239, 83, 80, 0.20)', stroke: 'rgba(239, 83, 80, 0.65)' },
};
const BREAKER_FILL = {
  bullish: { fill: 'rgba(255, 152, 0, 0.14)', stroke: 'rgba(255, 183, 77, 0.90)' },
  bearish: { fill: 'rgba(255, 112, 67, 0.14)', stroke: 'rgba(255, 138, 101, 0.90)' },
};
const VI_FILL = {
  bullish: { fill: 'rgba(38, 166, 154, 0.08)', stroke: 'rgba(38, 166, 154, 0.42)' },
  bearish: { fill: 'rgba(239, 83, 80, 0.08)', stroke: 'rgba(239, 83, 80, 0.42)' },
};
const OTE_FILL = { fill: 'rgba(66, 165, 245, 0.12)', stroke: 'rgba(66, 165, 245, 0.72)' };
const PD_EQ_LINE = 'rgba(255, 202, 40, 0.96)';
const PD_PREMIUM_FILL = { fill: 'rgba(255, 82, 82, 0.13)', stroke: 'rgba(255, 138, 128, 0.72)' };
const PD_DISCOUNT_FILL = { fill: 'rgba(0, 188, 212, 0.13)', stroke: 'rgba(77, 208, 225, 0.72)' };
const OPENING_GAP_FILL = {
  nwog: { fill: 'rgba(255, 193, 7, 0.15)', stroke: 'rgba(255, 213, 79, 0.88)' },
  ndog: { fill: 'rgba(255, 224, 130, 0.09)', stroke: 'rgba(255, 241, 118, 0.62)' },
};
const SESSION_BOX = {
  asia: { fill: 'rgba(0, 188, 212, 0.09)', fillStrong: 'rgba(0, 188, 212, 0.15)', stroke: 'rgba(77, 208, 225, 0.88)' },
  london_open: { fill: 'rgba(171, 71, 188, 0.08)', fillStrong: 'rgba(171, 71, 188, 0.14)', stroke: 'rgba(206, 147, 216, 0.84)' },
  new_york_open: { fill: 'rgba(38, 166, 154, 0.08)', fillStrong: 'rgba(38, 166, 154, 0.14)', stroke: 'rgba(128, 203, 196, 0.84)' },
  london_close: { fill: 'rgba(255, 167, 38, 0.08)', fillStrong: 'rgba(255, 167, 38, 0.14)', stroke: 'rgba(255, 204, 128, 0.84)' },
};
const PO3_STAGE_BOX = {
  accumulation: { fill: 'rgba(0, 188, 212, 0.10)', stroke: 'rgba(77, 208, 225, 0.86)' },
  manipulation: { fill: 'rgba(255, 112, 67, 0.13)', stroke: 'rgba(255, 138, 101, 0.92)' },
  distributionBull: { fill: 'rgba(38, 166, 154, 0.10)', stroke: 'rgba(38, 166, 154, 0.86)' },
  distributionBear: { fill: 'rgba(239, 83, 80, 0.10)', stroke: 'rgba(239, 83, 80, 0.86)' },
};
function fvgRectStyle(f: Fvg) {
  switch (normalizeFvgState(f.state)) {
    case 'active':
      return { ...FVG_FILL[f.direction], dashed: true, visible: true };
    case 'mitigated_50':
      return {
        fill: f.direction === 'bullish' ? 'rgba(38, 166, 154, 0.06)' : 'rgba(239, 83, 80, 0.06)',
        stroke: FVG_FILL[f.direction].stroke,
        dashed: true,
        visible: true,
      };
    case 'filled':
      return { fill: 'transparent', stroke: '', visible: false };
    case 'inverted_active':
      return {
        fill: FVG_FILL[f.direction].fill,
        stroke: FVG_FILL[f.direction].stroke,
        strokeWidth: FVG_INV_BORDER_WIDTH,
        dashed: false,
        visible: true,
      };
    case 'inverted_mitigated':
      return { fill: 'transparent', stroke: '', visible: false };
  }
}

function normalizeFvgState(state: Fvg['state'] | string): Fvg['state'] {
  switch (state) {
    case 'mitigated50': return 'mitigated_50';
    case 'invertedActive': return 'inverted_active';
    case 'invertedMitigated': return 'inverted_mitigated';
    default: return state as Fvg['state'];
  }
}

function obRectStyle(o: OrderBlock) {
  if (o.state === 'mitigated') return { fill: 'transparent', stroke: '', visible: false };
  const base = OB_FILL[o.direction];
  if (o.state === 'tested') {
    return { fill: base.fill.replace('0.20', '0.10'), stroke: base.stroke, dashed: false, visible: true };
  }
  return { ...base, dashed: false, visible: true };
}

export type OverlayFilter = {
  fvgStates: Set<Fvg['state']>;
  showFvg: boolean;
  showOb: boolean;
  showMss: boolean;
  showCisd: boolean;
  forceIndicators: boolean;
  showPdhPdl: boolean;
  showLiquidity: boolean;
  showSwingSweeps: boolean;
  showEqhEql: boolean;
  showPdhPdlSweeps: boolean;
  showLiquidityReversal: boolean;
  showLiquidityReversalCisd: boolean;
  showLiquidityReversalMss: boolean;
  showBos: boolean;
  showBreaker: boolean;
  showVi: boolean;
  showOte: boolean;
  showPremiumDiscount: boolean;
  showPremiumDiscountEqLine: boolean;
  showPremiumDiscountZones: boolean;
  showOpeningGaps: boolean;
  showNwog: boolean;
  showNdog: boolean;
  showOpeningGapActive: boolean;
  showOpeningGapMitigated: boolean;
  showOpeningGapFilled: boolean;
  showSessions: boolean;
  showSessionBoxes: boolean;
  showSessionLabels: boolean;
  showSessionBackground: boolean;
  showSessionHighLow: boolean;
  showSessionAsia: boolean;
  showSessionLondonOpen: boolean;
  showSessionNewYorkOpen: boolean;
  showSessionLondonClose: boolean;
  showPo3: boolean;
  showPo3Markers: boolean;
  showPo3StageBoxes: boolean;
  showPo3AccumulationStage: boolean;
  showPo3ManipulationStage: boolean;
  showPo3DistributionStage: boolean;
};

export const DEFAULT_FILTER: OverlayFilter = {
  fvgStates: new Set<Fvg['state']>(['active', 'mitigated_50', 'inverted_active']),
  showFvg: true,
  showOb: true,
  showMss: true,
  showCisd: true,
  forceIndicators: false,
  showPdhPdl: true,
  showLiquidity: true,
  showSwingSweeps: true,
  showEqhEql: true,
  showPdhPdlSweeps: true,
  showLiquidityReversal: true,
  showLiquidityReversalCisd: true,
  showLiquidityReversalMss: true,
  showBos: true,
  showBreaker: true,
  showVi: true,
  showOte: true,
  showPremiumDiscount: true,
  showPremiumDiscountEqLine: true,
  showPremiumDiscountZones: false,
  showOpeningGaps: true,
  showNwog: true,
  showNdog: true,
  showOpeningGapActive: true,
  showOpeningGapMitigated: true,
  showOpeningGapFilled: false,
  showSessions: true,
  showSessionBoxes: true,
  showSessionLabels: true,
  showSessionBackground: false,
  showSessionHighLow: false,
  showSessionAsia: true,
  showSessionLondonOpen: true,
  showSessionNewYorkOpen: true,
  showSessionLondonClose: true,
  showPo3: true,
  showPo3Markers: true,
  showPo3StageBoxes: true,
  showPo3AccumulationStage: true,
  showPo3ManipulationStage: true,
  showPo3DistributionStage: true,
};

export class StructureOverlay {
  /** Authoritative cache of every structure delivered by the backend
   *  for the current (symbol,tf) target. Filter changes do NOT evict
   *  entries from here — only `Invalidated` events / `clear()` do.
   *  This is what `repaintAll` / re-enable iterates over. */
  private allStructures: Map<string, IctStructure> = new Map();
  private prims: Map<string, PrimEntry> = new Map();
  private priceLines: Map<string, LevelEntry> = new Map();
  private markersApi: ReturnType<typeof createSeriesMarkers<Time>> | null = null;
  private markerFlushRaf = 0;
  private markerStructures: Map<string, MarkerStructure> = new Map();
  private filter: OverlayFilter = { ...DEFAULT_FILTER };
  private smtEnabled: boolean = true;
  private candidateEnabled: boolean = true;
  private smtChainEnabled: boolean = true;
  private smtHtfPdaEnabled: boolean = true;
  private smtSweepLineEnabled: boolean = true;
  private smtStructures: Map<string, SmtDivergence> = new Map();
  private smtSweepPrims: Map<string, SmtSweepPrimitive> = new Map();
  private chart: IChartApi;
  private series: ISeriesApi<'Candlestick'>;
  private targetSymbol: string;
  private targetTf: string;
  private barSeconds: number;
  /** Number of most-recent bars whose structures are kept on screen.
   *  Anything older is removed so the chart doesn't accumulate
   *  thousands of long historical bars / clustered markers. */
  private visibleBars: number;
  /** Latest closed-bar ts (ms) on the active series. FVG/OB rectangles
   *  extend their right edge to this ts so they reach "current price"
   *  visually instead of stopping at an arbitrary `ts_confirm + N×bar`
   *  fallback. ChartView updates this whenever a `bar:closed` arrives or
   *  when history loads. Falls back to `ts_confirm + 2 bars` if not set
   *  (e.g. very first paint before any bar event). */
  private lastBarTsMs: number = 0;
  private visibleLeftTsMs: number = 0;
 private visibleRightTsMs: number = 0;
 /** When set (candidate/alert clicked), CISD/MSS are only shown if
  *  break_ts falls within [start, end]. Bypasses isWithinVisibleWindow
  *  so old reversal signals inside the range still render. */
 private highlightRange: { start: number; end: number } | null = null;
 private highlightConfirm: HighlightConfirm | null = null;
 private reversalFocusActive: boolean = false;
 private highlightedSmtId: string | null = null;
 private pinnedSmtId: string | null = null;
private barTimesSec: number[] = [];
 private barIndexBySec: Map<number, number> = new Map();
  /** OHLC (high/low) keyed by bar-open second, kept in sync with
   *  `barTimesSec`. Used to place SMT sweep-line endpoints on real
   *  K-line extremes (§5.8) instead of a computed grid. */
  private barOhlcBySec: Map<number, { high: number; low: number }> = new Map();

  constructor(
    chart: IChartApi,
    series: ISeriesApi<'Candlestick'>,
    host: HTMLElement,
    symbol: string,
    tf: string,
  ) {
    this.chart = chart;
    void this.chart;
    this.series = series;
    this.targetSymbol = symbol;
    this.targetTf = tf;
    this.barSeconds = tfToSeconds(tf);
    this.visibleBars = tfVisibleBars(tf);
    this.markersApi = createSeriesMarkers<Time>(series, []);
    void host;
  }


  /** Returns true when the structure's anchor timestamp is recent
   *  enough to keep on the chart (within the last `visibleBars` on
   *  this TF). Cross-TF kinds (pdh/pdl) bypass — they have
   *  their own validity windows. */
  private isWithinVisibleWindow(anchorTsMs: number): boolean {
    if (!this.lastBarTsMs) return true; // no anchor yet — let it through
    const windowMs = this.barSeconds * 1000 * this.visibleBars;
    return anchorTsMs >= this.lastBarTsMs - windowMs;
  }

  setFilter(f: Partial<OverlayFilter>) {
    this.filter = { ...this.filter, ...f, fvgStates: f.fvgStates ?? this.filter.fvgStates };
    this.repaintAll();
  }
 getFilter() { return this.filter; }

  setHighlightRange(
    range: { start: number; end: number } | null,
    confirm: HighlightConfirm | null = null,
    focusActive: boolean = range !== null,
  ) {
    this.highlightRange = range;
    this.highlightConfirm = confirm;
    this.reversalFocusActive = focusActive;
    this.repaintAll();
  }

  /** Temporarily inject an audit-only SMT (including Invalidated) so clicking
   *  its Inbox row restores chain markers, PDA and sweep line for review.
   *  `highlightedSmtId` also owns the temporary yellow MTF PDA overlay;
   *  `pinnedSmtId` deliberately does not, so a chart click dismisses that
   *  overlay together with the yellow C1-C3 navigation rectangles. */
  setHighlightedSmt(smt: SmtDivergence | null) {
    const previous = this.pinnedSmtId;
    this.highlightedSmtId = smt?.id ?? null;
    // Dismissing the navigation rectangle must not dismiss the evidence the
    // user just opened. Keep that SMT pinned until another inbox context is
    // selected (or the pane target changes).
    if (smt) this.pinnedSmtId = smt.id;
    if (smt && previous && previous !== smt.id) {
      const prior = this.allStructures.get(previous);
      if (prior?.kind === 'smt_divergence'
          && prior.chains.some((chain) => chain.detection_state === 'invalidated')) {
        this.allStructures.delete(previous);
        this.smtStructures.delete(previous);
        this.removeById(previous);
        this.removeSweepSeries(`${previous}:smt_sweep`);
      }
    }
    if (smt) this.allStructures.set(smt.id, smt);
    this.repaintAll();
  }

  clearPinnedSmt() {
    const previous = this.pinnedSmtId;
    this.pinnedSmtId = null;
    this.highlightedSmtId = null;
    if (previous) {
      const prior = this.allStructures.get(previous);
      if (prior?.kind === 'smt_divergence'
          && prior.chains.some((chain) => chain.detection_state === 'invalidated')) {
        this.allStructures.delete(previous);
        this.smtStructures.delete(previous);
        this.removeById(previous);
        this.removeSweepSeries(`${previous}:smt_sweep`);
      }
    }
    this.repaintAll();
  }

  private isFocusedSmt(id: string) {
    return this.highlightedSmtId === id || this.pinnedSmtId === id;
  }

  setSmtEnabled(opts: { enabled: boolean; chain: boolean; htfPda: boolean; sweepLine: boolean }) {
    this.smtEnabled = opts.enabled;
    this.smtChainEnabled = opts.chain;
    this.smtHtfPdaEnabled = opts.htfPda;
    this.smtSweepLineEnabled = opts.sweepLine;
    this.repaintAll();
  }

  setCandidateEnabled(enabled: boolean) {
    this.candidateEnabled = enabled;
  }

  isCandidateEnabled(): boolean {
    return this.candidateEnabled;
  }

  setTarget(symbol: string, tf: string) {
    this.targetSymbol = symbol;
    this.targetTf = tf;
    this.barSeconds = tfToSeconds(tf);
    this.visibleBars = tfVisibleBars(tf);
    this.barTimesSec = [];
    this.barIndexBySec.clear();
    this.barOhlcBySec.clear();
   this.clear();
  }

  setBarTimes(bars: { time: number; high: number; low: number }[]) {
    const valid = bars.filter((b) => Number.isFinite(b.time) && Number.isFinite(b.high) && Number.isFinite(b.low));
    this.barTimesSec = Array.from(new Set(valid.map((b) => Math.floor(b.time)))).sort((a, b) => a - b);
    this.barIndexBySec = new Map(this.barTimesSec.map((t, i) => [t, i]));
    this.barOhlcBySec = new Map(valid.map((b) => [Math.floor(b.time), { high: b.high, low: b.low }]));
    this.rebuildPo3StageBoxes();
   for (const item of this.allStructures.values()) {
     if (item.kind === 'fvg' || item.kind === 'order_block' || item.kind === 'breaker_block' || item.kind === 'volume_imbalance' || item.kind === 'ote' || item.kind === 'premium_discount' || item.kind === 'session_range' || item.kind === 'kill_zone_window' || item.kind === 'mss' || item.kind === 'cisd' || item.kind === 'bos' || item.kind === 'liquidity_sweep' || item.kind === 'equal_highs_lows' || item.kind === 'liquidity_reversal') this.upsertStructure(item);
   }
    // SMT sweep lines snap endpoints to real bar OHLC (§5.8), so
    // re-evaluate them whenever the bar grid (re)loads - bars may
    // arrive after the SMT structures and the endpoints need the
    // OHLC to place prices on the K-line top/bottom.
    const smtEligible = this.smtSweepEligible();
    for (const ss of this.smtStructures.values()) {
      this.upsertSmtSweepLine(ss, smtEligible);
    }
 }

  addBarTime(tsMs: number, high: number, low: number) {
    const sec = Math.floor(tsMs / 1000);
    if (!Number.isFinite(sec)) return;
    let insertedPast = false;
    if (!this.barIndexBySec.has(sec)) {
      let lo = 0;
      let hi = this.barTimesSec.length;
      while (lo < hi) {
        const mid = (lo + hi) >>> 1;
        if (this.barTimesSec[mid] < sec) lo = mid + 1;
        else hi = mid;
      }
      insertedPast = lo < this.barTimesSec.length;
      this.barTimesSec.splice(lo, 0, sec);
      for (let i = lo; i < this.barTimesSec.length; i += 1) {
        this.barIndexBySec.set(this.barTimesSec[i], i);
      }
    }
    this.barOhlcBySec.set(sec, { high, low });
    if (insertedPast) {
      // Inserting a quote-free minute shifts later logical coordinates.
      // Re-anchor cached boxes/lines just as a corrected history load does.
      this.setBarTimes(this.barTimesSec.map((time) => ({ time, ...this.barOhlcBySec.get(time)! })));
    }
  }

  private logicalIndexForMs(tsMs: number): number | undefined {
    const sec = Math.floor(tsMs / 1000);
    const exact = this.barIndexBySec.get(sec);
    if (typeof exact === 'number') return exact;
    if (!this.barTimesSec.length) return undefined;
    if (sec < this.barTimesSec[0] || sec > this.barTimesSec[this.barTimesSec.length - 1]) {
      return undefined;
    }
    let lo = 0;
    let hi = this.barTimesSec.length - 1;
    while (lo <= hi) {
      const mid = Math.floor((lo + hi) / 2);
      if (this.barTimesSec[mid] < sec) lo = mid + 1;
      else hi = mid - 1;
    }
    return Math.max(0, Math.min(this.barTimesSec.length - 1, lo));
  }

  /** Right boundary for a zone that is still live.
   *
   * Do not point an active FVG/OB at `confirm + N bars`: that timestamp may
   * not exist in lightweight-charts yet. Bind it to the boundary immediately
   * after the latest real bar instead, so both rectangle endpoints always
   * have stable logical coordinates while the current candle is forming. */
  private liveZoneRightEdge(fallbackEndMs: number): { endMs: number; endLogical?: number } {
    if (this.barTimesSec.length > 0) {
      const lastIndex = this.barTimesSec.length - 1;
      const lastOpenSec = this.barTimesSec[lastIndex];
      return {
        endMs: (lastOpenSec + this.barSeconds) * 1000,
        endLogical: lastIndex + 1,
      };
    }
    return { endMs: fallbackEndMs, endLogical: undefined };
  }

  /** Expose the pane's canonical bar index to navigation primitives. A
   *  timestamp-only rectangle can temporarily lose its X coordinate while
   *  lightweight-charts synchronizes/zooms multiple panes; binding both
   *  endpoints to real logical bars keeps its candle span stable. */
  logicalIndexForTimestamp(tsMs: number): number | undefined {
    return this.logicalIndexForMs(tsMs);
  }

  private markerTimeSecForMs(tsMs: number): number | undefined {
    const sec = Math.floor(tsMs / 1000);
    if (!Number.isFinite(sec)) return undefined;
    if (!this.barTimesSec.length) return sec;
    const exact = this.barIndexBySec.get(sec);
    if (typeof exact === 'number') return this.barTimesSec[exact];
    const first = this.barTimesSec[0];
    const last = this.barTimesSec[this.barTimesSec.length - 1];
    if (sec < first || sec >= last + this.barSeconds) return undefined;
    let lo = 0;
    let hi = this.barTimesSec.length - 1;
    while (lo <= hi) {
      const mid = Math.floor((lo + hi) / 2);
      if (this.barTimesSec[mid] <= sec) lo = mid + 1;
      else hi = mid - 1;
    }
   return this.barTimesSec[Math.max(0, Math.min(this.barTimesSec.length - 1, hi))];
 }

  /** Find the bar at or before `tsMs` on the current pane and return its
   *  open-second + OHLC. Used to place SMT sweep-line endpoints on real
   *  K-line extremes (§5.8): the 4h bar grid is offset from UTC midnight,
   *  so a math `floor(ts / 4h)` lands on a non-existent bar; looking up
   *  the actual bar grid avoids that and yields true high/low values. */
  private barAtOrBefore(tsMs: number): { timeSec: number; high: number; low: number } | undefined {
    const sec = Math.floor(tsMs / 1000);
    if (!Number.isFinite(sec) || !this.barTimesSec.length) return undefined;
    const exact = this.barIndexBySec.get(sec);
    let idx: number;
    if (typeof exact === 'number') {
      idx = exact;
    } else {
      if (sec < this.barTimesSec[0]) return undefined;
      let lo = 0;
      let hi = this.barTimesSec.length - 1;
      while (lo <= hi) {
        const mid = Math.floor((lo + hi) / 2);
        if (this.barTimesSec[mid] <= sec) lo = mid + 1;
        else hi = mid - 1;
      }
      idx = hi;
      if (idx < 0) return undefined;
    }
    const timeSec = this.barTimesSec[idx];
    const ohlc = this.barOhlcBySec.get(timeSec);
    if (!ohlc) return undefined;
    return { timeSec, high: ohlc.high, low: ohlc.low };
  }

 /** Updates the right-edge anchor for FVG/OB rectangles. Call on every
   *  bar event so structures stay glued to the latest bar AND so the
   *  visible-window gate re-evaluates: structures that just rolled
   *  outside the last `visibleBars` window get removed automatically. */
  setLastBarTs(ms: number) {
    if (!ms || ms === this.lastBarTsMs) return;
    this.lastBarTsMs = ms;
    this.rebuildPo3StageBoxes();
    for (const item of this.allStructures.values()) {
      // Re-emit through upsertStructure so the visible-window gate
      // (and right-edge anchor for FVG/OB) re-applies on every bar.
      if (item.kind === 'fvg' || item.kind === 'order_block' || item.kind === 'breaker_block' || item.kind === 'volume_imbalance' || item.kind === 'ote' || item.kind === 'premium_discount' || item.kind === 'nwog' || item.kind === 'ndog' || item.kind === 'session_range' || item.kind === 'kill_zone_window' || item.kind === 'liquidity_sweep' || item.kind === 'equal_highs_lows' || item.kind === 'liquidity_reversal') {
        this.upsertStructure(item);
      }
    }
  }

  setVisibleTimeRange(fromSec?: number, toSec?: number) {
    const nextLeft = typeof fromSec === 'number' && Number.isFinite(fromSec) ? Math.floor(fromSec * 1000) : 0;
    const nextRight = typeof toSec === 'number' && Number.isFinite(toSec) ? Math.floor(toSec * 1000) : 0;
    if (nextLeft === this.visibleLeftTsMs && nextRight === this.visibleRightTsMs) return;
    this.visibleLeftTsMs = nextLeft;
    this.visibleRightTsMs = nextRight;
    for (const item of this.allStructures.values()) {
      if (item.kind === 'liquidity_sweep' || item.kind === 'liquidity_reversal') {
        this.upsertStructure(item);
      }
    }
  }

  private isSweepSegmentVisible(levelTsMs: number, sweepTsMs: number): boolean {
    if (!this.visibleLeftTsMs || !this.visibleRightTsMs) return true;
    const start = Math.min(levelTsMs, sweepTsMs);
    const end = Math.max(levelTsMs, sweepTsMs);
    return end >= this.visibleLeftTsMs && start <= this.visibleRightTsMs;
  }

  applyEvent(ev: StructureEvent) {
    if (ev.op === 'invalidated') {
      this.allStructures.delete(ev.id);
      this.removeById(ev.id);
      return;
    }
    // Cache before filter evaluation so re-enabling shows it again.
    this.allStructures.set(ev.id, ev as unknown as IctStructure);
    if (!this.matchesTarget(ev)) {
      return;
    }
    this.upsert(ev);
  }

  /** Applies the authoritative snapshot from `list_structures`. */
  applySnapshot(rows: IctStructure[]) {
    // Snapshot is authoritative for every structure owned by this pane, not
    // just SMT. Reconcile missed invalidations after sleep/reconnect so stale
    // FVG/CISD/MSS/etc. cannot survive forever in the overlay cache.
    const activeIds = new Set(rows.map((row) => row.id));
    const staleIds = [...this.allStructures.values()]
      .filter((row) => this.belongsToTarget(row))
      .filter((row) => !(row.kind === 'smt_divergence' && this.isFocusedSmt(row.id)))
      .filter((row) => !activeIds.has(row.id))
      .map((row) => row.id);
    for (const id of staleIds) {
      this.allStructures.delete(id);
      this.removeById(id);
    }

    for (const row of rows) {
      this.allStructures.set(row.id, row);
    }
    this.rebuildPo3StageBoxes();
    for (const row of rows) {
      if (!this.structureMatchesTarget(row)) continue;
      if (row.kind === 'power_of_3') continue;
      this.upsertStructure(row);
    }
  }

  clear() {
    for (const { prim } of this.prims.values()) {
      try { this.series.detachPrimitive(prim); } catch { /* ignore */ }
    }
    this.allStructures.clear();
    this.prims.clear();
    for (const { line } of this.priceLines.values()) {
      try { this.series.removePriceLine(line); } catch { /* ignore */ }
    }
    for (const { autoscalePrim } of this.priceLines.values()) {
      try { this.series.detachPrimitive(autoscalePrim); } catch { /* ignore */ }
    }
    this.priceLines.clear();
    this.markerStructures.clear();
    this.smtStructures.clear();
    // Remove the old pane's persistent SMT layer. A manual pin is retained in
    // state and is re-injected by ChartPane when the selected SMT is also
    // meaningful on the destination TF.
    for (const prim of this.smtSweepPrims.values()) {
      try { this.series.detachPrimitive(prim); } catch { /* ignore */ }
    }
    this.smtSweepPrims.clear();
    this.markersApi?.setMarkers([]);
  }

  private matchesTarget(ev: StructureEvent): boolean {
    if (ev.op === 'invalidated') return true;
    return this.structureMatchesTarget(ev);
  }

  private structureMatchesTarget(s: IctStructure): boolean {
    if (s.kind === 'kill_zone') return false;
    if (s.kind === 'smt_divergence') {
      if (!this.smtEnabled) return false;
      if (!this.smtChainEnabled && !this.smtHtfPdaEnabled && !this.smtSweepLineEnabled) return false;
      return this.belongsToTarget(s);
    }
    return this.belongsToTarget(s);
  }

  private belongsToTarget(s: IctStructure): boolean {
    if (s.kind === 'kill_zone') return false;
    if (s.kind === 'smt_divergence') {
      // Historical rows may predate the all-symbol chain readiness gate.
      // Membership is defined by the SMT symbol set / liquidity evidence;
      // chain markers still render only when that pane has a real chain.
      const includesTarget = s.symbol_set.includes(this.targetSymbol)
        || s.liquidity_refs.some((liquidity) => liquidity.symbol === this.targetSymbol)
        || s.chains.some((chain) => chain.symbol === this.targetSymbol);
      return includesTarget
        && (s.comparison_timeframe === this.targetTf || s.context_timeframe === this.targetTf);
    }
    if (s.kind === 'pdh' || s.kind === 'pdl' || s.kind === 'nwog' || s.kind === 'ndog' || s.kind === 'session_range' || s.kind === 'kill_zone_window') return s.symbol === this.targetSymbol;
    return s.symbol === this.targetSymbol && s.tf === this.targetTf;
  }

  private upsert(ev: StructureEvent) {
    if (ev.op === 'invalidated') return;
    this.upsertStructure(ev);
  }

  private upsertStructure(s: IctStructure) {
    switch (s.kind) {
      case 'fvg':
        return this.upsertFvg(s);
      case 'order_block':
        return this.upsertOb(s);
      case 'breaker_block':
        return this.upsertBreaker(s);
      case 'volume_imbalance':
        return this.upsertVi(s);
      case 'ote':
        return this.upsertOte(s);
      case 'premium_discount':
        return this.upsertPremiumDiscount(s);
      case 'nwog':
      case 'ndog':
        return this.upsertOpeningGap(s);
      case 'session_range':
        return this.upsertSessionRange(s);
      case 'kill_zone_window':
        return this.upsertKillZoneWindow(s);
      case 'power_of_3':
        return this.rebuildPo3StageBoxes();
      case 'mss':
        return this.upsertMss(s);
      case 'bos':
        return this.upsertBos(s);
      case 'cisd':
        return this.upsertCisd(s);
      case 'pdh':
      case 'pdl':
        return this.upsertLevel(s);
      case 'kill_zone':
        return;
      case 'liquidity_sweep':
        return this.upsertLiquiditySweep(s);
      case 'equal_highs_lows':
        return this.upsertEqhEql(s);
      case 'liquidity_reversal':
        return this.upsertLiquidityReversal(s);
      case 'smt_divergence':
        return this.upsertSmt(s);
    }
  }

  private removeById(id: string) {
    const relatedPrimIds = [`${id}:line`, `${id}:premium`, `${id}:discount`, `${id}:sweep`, `${id}:confirm`, `${id}:high`, `${id}:low`, `${id}:bg`, `${id}:accumulation`, `${id}:manipulation`, `${id}:distribution`, `${id}:entry`];
    let removedPrim = false;
    for (const k of [id, ...relatedPrimIds]) {
      const e = this.prims.get(k);
      if (e) {
        try { this.series.detachPrimitive(e.prim); } catch { /* ignore */ }
        this.prims.delete(k);
        removedPrim = true;
      }
    }
    const lvl = this.priceLines.get(id);
    let removedLine = false;
    if (lvl) {
      try { this.series.removePriceLine(lvl.line); } catch { /* ignore */ }
      try { this.series.detachPrimitive(lvl.autoscalePrim); } catch { /* ignore */ }
      this.priceLines.delete(id);
      removedLine = true;
    }
    let removedMarker = false;
    const markerRemoved = this.markerStructures.delete(id);
    const smtRemoved = this.smtStructures.delete(id);
    if (markerRemoved || smtRemoved) {
      this.scheduleFlushMarkers();
      removedMarker = true;
    }
    this.removeSweepSeries(`${id}:smt_sweep`);
    if (smtRemoved) {
      // Rebuild: a shared PDA zone may need removal after this SMT goes.
      this.rebuildSmtPdaZones();
    }
    void removedPrim;
    void removedLine;
    void removedMarker;
  }

  private upsertFvg(f: Fvg) {
    const state = normalizeFvgState(f.state);
    if (!this.filter.showFvg || !this.filter.fvgStates.has(state)) {
      this.removeById(f.id);
      return;
    }
    // Visible-window gate: anchor at ts_confirm so newly-formed FVGs
    // immediately count even if their leg started long ago.
    if (!this.isWithinVisibleWindow(f.ts_confirm)) {
      this.removeById(f.id);
      return;
    }
    const start = (f.ts_open / 1000) as Time;
    // Right edge: consumed FVGs (used by SMT) are truncated at the exit
    // bar. Unconsumed FVGs extend to the latest bar (M3 behavior).
    const fallbackEndMs = f.ts_confirm + this.barSeconds * 1000 * 2;
    const activeEdge = this.liveZoneRightEdge(fallbackEndMs);
    const endMs = f.consumed_exit_ts ?? activeEdge.endMs;
    const end = (endMs / 1000) as Time;
    const spec: RectSpec = {
      startTime: start,
      endTime: end,
      startLogical: this.logicalIndexForMs(f.ts_open),
      endLogical: f.consumed_exit_ts
        ? this.logicalIndexForMs(endMs)
        : activeEdge.endLogical,
      priceLow: f.price_low,
      priceHigh: f.price_high,
      style: fvgRectStyle(f),
    };
    this.upsertRect(f.id, spec, f);
  }

  private upsertOb(o: OrderBlock) {
    if (!this.filter.showOb) { this.removeById(o.id); return; }
    if (!this.isWithinVisibleWindow(o.ts_confirm)) { this.removeById(o.id); return; }
    const start = (o.ts_open / 1000) as Time;
    const fallbackEndMs = o.ts_confirm + this.barSeconds * 1000 * 2;
    const activeEdge = this.liveZoneRightEdge(fallbackEndMs);
    const endMs = activeEdge.endMs;
    const end = (endMs / 1000) as Time;
    const spec: RectSpec = {
      startTime: start, endTime: end,
      startLogical: this.logicalIndexForMs(o.ts_open),
      endLogical: activeEdge.endLogical,
      priceLow: o.price_low, priceHigh: o.price_high,
      style: obRectStyle(o),
    };
    this.upsertRect(o.id, spec, o);
  }

  private upsertBreaker(b: BreakerBlock) {
    if (!this.filter.showBreaker || b.state === 'mitigated' || b.state === 'invalidated') { this.removeById(b.id); return; }
    if (!this.isWithinVisibleWindow(b.ts_confirm)) { this.removeById(b.id); return; }
    const fallbackEndMs = b.ts_confirm + this.barSeconds * 1000 * 2;
    const endMs = Math.max(this.lastBarTsMs || 0, fallbackEndMs);
    const base = BREAKER_FILL[b.direction];
    this.upsertRect(b.id, {
      startTime: (b.ts_open / 1000) as Time,
      endTime: (endMs / 1000) as Time,
      startLogical: this.logicalIndexForMs(b.ts_open),
      endLogical: this.logicalIndexForMs(endMs),
      priceLow: b.price_low,
      priceHigh: b.price_high,
      style: { ...base, strokeWidth: b.state === 'tested' ? 2 : 1.5, dashed: false, visible: true },
      label: 'Breaker',
    }, b);
  }

  private upsertVi(v: VolumeImbalance) {
    if (!this.filter.showVi || v.state === 'filled') { this.removeById(v.id); return; }
    if (!this.isWithinVisibleWindow(v.ts_confirm)) { this.removeById(v.id); return; }
    const fallbackEndMs = v.ts_confirm + this.barSeconds * 1000 * 2;
    const endMs = Math.max(this.lastBarTsMs || 0, fallbackEndMs);
    const base = VI_FILL[v.direction];
    this.upsertRect(v.id, {
      startTime: (v.ts_open / 1000) as Time,
      endTime: (endMs / 1000) as Time,
      startLogical: this.logicalIndexForMs(v.ts_open),
      endLogical: this.logicalIndexForMs(endMs),
      priceLow: v.price_low,
      priceHigh: v.price_high,
      style: { ...base, dashed: true, visible: true },
      label: 'VI',
    }, v);
  }

  private upsertOte(o: OteZone) {
    if (!this.filter.showOte) { this.removeById(o.id); return; }
    if (!this.isWithinVisibleWindow(o.leg_end_ts)) { this.removeById(o.id); return; }
    const fallbackEndMs = o.leg_end_ts + this.barSeconds * 1000 * 2;
    const endMs = Math.max(this.lastBarTsMs || 0, fallbackEndMs);
    const label = o.confluent_structure_ids.length ? 'OTE + confluence' : 'OTE 0.62–0.79';
    this.upsertRect(o.id, {
      startTime: (o.leg_start_ts / 1000) as Time,
      endTime: (endMs / 1000) as Time,
      startLogical: this.logicalIndexForMs(o.leg_start_ts),
      endLogical: this.logicalIndexForMs(endMs),
      priceLow: o.price_low,
      priceHigh: o.price_high,
      style: { ...OTE_FILL, dashed: true, visible: true },
      label,
    }, o);
  }

  private upsertPremiumDiscount(pd: PremiumDiscount) {
    if (!this.filter.showPremiumDiscount) { this.removeById(pd.id); return; }
    if (!this.isWithinVisibleWindow(pd.range_end_ts)) { this.removeById(pd.id); return; }
    const fallbackEndMs = pd.range_end_ts + this.barSeconds * 1000 * 2;
    const endMs = Math.max(this.lastBarTsMs || 0, fallbackEndMs);
    const label = pdSideLabel(pd.current_side);
    if (this.filter.showPremiumDiscountZones) {
      this.upsertRect(`${pd.id}:premium`, {
        startTime: (pd.range_start_ts / 1000) as Time,
        endTime: (endMs / 1000) as Time,
        startLogical: this.logicalIndexForMs(pd.range_start_ts),
        endLogical: this.logicalIndexForMs(endMs),
        priceLow: pd.equilibrium,
        priceHigh: pd.high,
        style: { ...PD_PREMIUM_FILL, dashed: false, visible: true },
        label: 'Premium',
      }, pd);
      this.upsertRect(`${pd.id}:discount`, {
        startTime: (pd.range_start_ts / 1000) as Time,
        endTime: (endMs / 1000) as Time,
        startLogical: this.logicalIndexForMs(pd.range_start_ts),
        endLogical: this.logicalIndexForMs(endMs),
        priceLow: pd.low,
        priceHigh: pd.equilibrium,
        style: { ...PD_DISCOUNT_FILL, dashed: false, visible: true },
        label: 'Discount',
      }, pd);
    } else {
      this.removeById(`${pd.id}:premium`);
      this.removeById(`${pd.id}:discount`);
    }
    if (!this.filter.showPremiumDiscountEqLine) { this.removeById(`${pd.id}:line`); return; }
    this.upsertRect(`${pd.id}:line`, {
      startTime: (pd.range_start_ts / 1000) as Time,
      endTime: (endMs / 1000) as Time,
      startLogical: this.logicalIndexForMs(pd.range_start_ts),
      endLogical: this.logicalIndexForMs(endMs),
      priceLow: pd.equilibrium,
      priceHigh: pd.equilibrium,
      autoscalePrice: pd.equilibrium,
      zOrder: 'top',
      style: { fill: PD_EQ_LINE, stroke: PD_EQ_LINE, strokeWidth: 1.25, dashed: true, visible: true },
      label: `EQ · ${label}`,
      endpointDots: 'none',
    }, pd);
  }

  private upsertOpeningGap(g: GapZone) {
    if (!this.filter.showOpeningGaps) { this.removeById(g.id); return; }
    if (g.kind === 'nwog' && !this.filter.showNwog) { this.removeById(g.id); return; }
    if (g.kind === 'ndog' && !this.filter.showNdog) { this.removeById(g.id); return; }
    if (g.state === 'active' && !this.filter.showOpeningGapActive) { this.removeById(g.id); return; }
    if (g.state === 'mitigated_50' && !this.filter.showOpeningGapMitigated) { this.removeById(g.id); return; }
    if (g.state === 'filled' && !this.filter.showOpeningGapFilled) { this.removeById(g.id); return; }
    const fallbackEndMs = g.ts_start + this.barSeconds * 1000 * 2;
    const endMs = Math.max(this.lastBarTsMs || 0, fallbackEndMs);
    const base = OPENING_GAP_FILL[g.kind];
    const opacityStyle = g.state === 'filled'
      ? { fill: base.fill.replace(/0\.\d+\)/, '0.035)'), stroke: base.stroke.replace(/0\.\d+\)/, '0.32)') }
      : g.state === 'mitigated_50'
        ? { fill: base.fill.replace(/0\.\d+\)/, '0.06)'), stroke: base.stroke.replace(/0\.\d+\)/, '0.48)') }
        : base;
    this.upsertRect(g.id, {
      startTime: (g.ts_start / 1000) as Time,
      endTime: (endMs / 1000) as Time,
      startLogical: this.logicalIndexForMs(g.ts_start),
      endLogical: this.logicalIndexForMs(endMs),
      priceLow: g.price_low,
      priceHigh: g.price_high,
      style: { ...opacityStyle, dashed: g.state !== 'active', visible: true },
      label: g.kind === 'nwog' ? 'NWOG' : 'NDOG',
    }, g);
  }

  private upsertSessionRange(r: SessionRange) {
    if (!this.filter.showSessions || !this.sessionEnabled(r.session)) {
      this.removeById(r.id);
      this.removeById(`${r.id}:high`);
      this.removeById(`${r.id}:low`);
      return;
    }
    const style = SESSION_BOX[r.session];
    const labelPrefix = sessionShortLabel(r.session);
    const activeEndMs = this.lastBarTsMs && this.lastBarTsMs >= r.ts_start && this.lastBarTsMs <= r.ts_end
      ? Math.max(this.lastBarTsMs, r.ts_start + this.barSeconds * 1000)
      : r.ts_end;
    const endMs = r.finalized ? r.ts_end : activeEndMs;
    const startLogical = this.logicalIndexForMs(r.ts_start);
    const endLogical = this.logicalIndexForMs(endMs);
    if (typeof startLogical !== 'number' || typeof endLogical !== 'number') {
      this.removeById(r.id);
      this.removeById(`${r.id}:high`);
      this.removeById(`${r.id}:low`);
      return;
    }
    if (this.filter.showSessionBoxes) {
      this.upsertRect(r.id, {
        startTime: (r.ts_start / 1000) as Time,
        endTime: (endMs / 1000) as Time,
        startLogical,
        endLogical,
        priceLow: r.low,
        priceHigh: r.high,
        style: {
          fill: this.filter.showSessionBackground ? style.fillStrong : style.fill,
          stroke: style.stroke,
          strokeWidth: r.session === 'asia' ? 1.25 : 1,
          dashed: !r.finalized,
          visible: true,
        },
        label: this.filter.showSessionLabels ? sessionBoxLabel(r.session) : undefined,
      }, r);
    } else {
      this.removeById(r.id);
    }
    if (!this.filter.showSessionHighLow) {
      this.removeById(`${r.id}:high`);
      this.removeById(`${r.id}:low`);
      return;
    }
    const common = {
      startTime: (r.ts_start / 1000) as Time,
      endTime: (endMs / 1000) as Time,
      startLogical,
      endLogical,
      style: { fill: style.stroke, stroke: style.stroke, strokeWidth: 1, dashed: true, visible: true },
      endpointDots: 'none' as const,
    };
    this.upsertRect(`${r.id}:high`, {
      ...common,
      priceLow: r.high,
      priceHigh: r.high,
      autoscalePrice: r.high,
      label: this.filter.showSessionLabels ? `${labelPrefix} H` : undefined,
    }, r);
    this.upsertRect(`${r.id}:low`, {
      ...common,
      priceLow: r.low,
      priceHigh: r.low,
      autoscalePrice: r.low,
      label: this.filter.showSessionLabels ? `${labelPrefix} L` : undefined,
    }, r);
  }

  private upsertKillZoneWindow(w: KillZoneWindow) {
    this.removeById(w.id);
    this.removeById(`${w.id}:bg`);
  }

  private sessionEnabled(session: SessionRange['session']): boolean {
    switch (session) {
      case 'asia': return this.filter.showSessionAsia;
      case 'london_open': return this.filter.showSessionLondonOpen;
      case 'new_york_open': return this.filter.showSessionNewYorkOpen;
      case 'london_close': return this.filter.showSessionLondonClose;
    }
  }

  private upsertRect(id: string, spec: RectSpec, structure: IctStructure) {
    const existing = this.prims.get(id);
    if (existing && existing.prim instanceof RectanglePrimitive) {
      existing.prim.setSpec(spec);
      existing.structure = structure;
      return;
    }
    if (existing) {
      try { this.series.detachPrimitive(existing.prim); } catch { /* ignore */ }
      this.prims.delete(id);
    }
    const prim = new RectanglePrimitive(spec);
    try { this.series.attachPrimitive(prim); } catch (e) { console.warn('attachPrimitive failed', e); return; }
    this.prims.set(id, { id, prim, structure });
  }

  private upsertLevel(m: LevelMarker) {
    if (!this.filter.showPdhPdl) {
      this.removeById(m.id);
      return;
    }
    // De-dupe by (symbol, label): there must be at most ONE active PDH
    // and ONE active PDL per symbol on the chart at any moment. The
    // backend can legitimately emit several PDH/PDL events when
    // apply_detector_param replays history (each prev-day rollover
    // produces a New(pdh)+New(pdl) pair), and any out-of-order
    // delivery between those Invalidated and New events leaves stale
    // priceLines attached to lwc whose axis labels then visually
    // overlap the canonical pair — empirically lwc v5 stops
    // rendering the older one entirely once it loses its label slot,
    // which is the "1m sees PDL but not PDH" bug user reported on
    // 2026-06-16. Removing siblings up-front guarantees a single
    // PDH+PDL pair regardless of event ordering.
    {
      const stale: string[] = [];
      for (const [otherId, entry] of this.priceLines.entries()) {
        if (otherId === m.id) continue;
        if (entry.level.symbol === m.symbol && entry.level.label === m.label) {
          stale.push(otherId);
        }
      }
      for (const otherId of stale) {
        const entry = this.priceLines.get(otherId)!;
        try { this.series.removePriceLine(entry.line); } catch { /* ignore */ }
        try { this.series.detachPrimitive(entry.autoscalePrim); } catch { /* ignore */ }
        this.priceLines.delete(otherId);
        this.allStructures.delete(otherId);
      }
    }
    const existing = this.priceLines.get(m.id);
    const color = cssVar('--liquidity', '#ffd54f');
    const opts = {
      price: m.price,
      color,
      lineStyle: LineStyle.Dashed,
      lineWidth: 1 as const,
      // Single label only: keep the axis tag on the right price scale and
      // drop the inline `title` (which would render an extra "PDH ..."
      // chip on the line). PDH always sits above PDL, both in `--liquidity`
      // yellow, so the kind is identifiable from position + color.
      axisLabelVisible: true,
      title: '',
    };
    if (existing) {
      existing.line.applyOptions(opts);
      existing.level = m;
      existing.autoscalePrim.setSpec(this.levelAutoscaleSpec(m));
      return;
    }
    const line = this.series.createPriceLine(opts);
    const autoscalePrim = new RectanglePrimitive(this.levelAutoscaleSpec(m));
    try { this.series.attachPrimitive(autoscalePrim); } catch (e) { console.warn('attach level autoscale primitive failed', e); }
    this.priceLines.set(m.id, { line, level: m, autoscalePrim });
  }

  private levelAutoscaleSpec(m: LevelMarker): RectSpec {
    const anchor = this.lastBarTsMs || m.valid_from_ts || Date.now();
    const start = (anchor / 1000) as Time;
    const end = ((anchor + this.barSeconds * 1000) / 1000) as Time;
    return {
      startTime: start,
      endTime: end,
      priceLow: m.price,
      priceHigh: m.price,
      style: { fill: 'transparent', stroke: '', visible: false },
      autoscalePrice: m.price,
    };
  }

 private upsertMss(m: Mss) {
    if (this.reversalFocusActive) {
      if (!this.highlightRange
          || m.break_ts < this.highlightRange.start
          || m.break_ts > this.highlightRange.end) {
        this.removeById(m.id);
        return;
      }
    } else if (!this.filter.showMss || !this.isWithinVisibleWindow(m.break_ts)) {
      this.removeById(m.id);
      return;
    }
    this.markerStructures.set(m.id, m);
    this.scheduleFlushMarkers();
    const mssRightBars = 5;
    this.upsertTriggerLine(`${m.id}:line`, m, (m.break_ts / 1000) as Time, m.swing_price, structureLineColor(), true, mssRightBars);
  }

private upsertCisd(c: Cisd) {
    if (this.reversalFocusActive) {
      if (!this.highlightRange
          || c.break_ts < this.highlightRange.start
          || c.break_ts > this.highlightRange.end) {
        this.removeById(c.id);
        return;
      }
    } else if (!this.filter.showCisd || !this.isWithinVisibleWindow(c.break_ts)) {
      this.removeById(c.id);
      return;
    }
    this.markerStructures.set(c.id, c);
    this.scheduleFlushMarkers();
    const cisdRightBars = 5;
    this.upsertTriggerLine(`${c.id}:line`, c, (c.break_ts / 1000) as Time, c.leg_origin_price, structureLineColor(), false, cisdRightBars);
  }

  private upsertBos(b: Bos) {
    if (!this.filter.showBos) { this.removeById(b.id); return; }
    if (!this.isWithinVisibleWindow(b.break_ts)) { this.removeById(b.id); return; }
    this.markerStructures.set(b.id, b);
    this.scheduleFlushMarkers();
    this.upsertTriggerLine(`${b.id}:line`, b, (b.break_ts / 1000) as Time, b.swing_price, structureLineColor(), true, 4);
  }

  private upsertLiquiditySweep(s: LiquiditySweep) {
    if (!this.filter.showLiquidity || !this.sweepKindEnabled(s)) {
      this.removeById(s.id);
      return;
    }
    if (!this.isSweepSegmentVisible(s.level_ts, s.sweep_ts)) {
      this.removeById(s.id);
      return;
    }
    if (!this.isPreferredSweep(s)) {
      this.removeById(s.id);
      return;
    }
    this.removeLowerPriorityDuplicateSweeps(s);
    this.markerStructures.set(s.id, s);
    this.scheduleFlushMarkers();
    this.upsertSweepLink(s);
  }

  private isPreferredSweep(s: LiquiditySweep): boolean {
    for (const item of this.allStructures.values()) {
      if (item.kind !== 'liquidity_sweep' || item.id === s.id) continue;
      if (!sameSweepCluster(s, item)) continue;
      if (sweepPriority(item) > sweepPriority(s)) return false;
    }
    return true;
  }

  private removeLowerPriorityDuplicateSweeps(s: LiquiditySweep) {
    for (const item of this.allStructures.values()) {
      if (item.kind !== 'liquidity_sweep' || item.id === s.id) continue;
      if (!sameSweepCluster(s, item)) continue;
      if (sweepPriority(item) < sweepPriority(s)) this.removeById(item.id);
    }
  }

  private upsertSweepLink(s: LiquiditySweep) {
    const color = sweepColor(s);
    this.upsertRect(`${s.id}:line`, {
      startTime: (s.level_ts / 1000) as Time,
      endTime: (s.sweep_ts / 1000) as Time,
      priceLow: s.level_price,
      priceHigh: s.level_price,
      autoscalePrice: s.level_price,
      endpointDots: 'both',
      zOrder: 'top',
      style: { fill: color, stroke: color, strokeWidth: 1.75, dashed: true, visible: true },
    }, s);
  }

  private upsertEqhEql(e: EqualHighsLows) {
    if (!this.filter.showLiquidity || !this.filter.showEqhEql) { this.removeById(e.id); return; }
    if (!this.isWithinVisibleWindow(e.ts_end)) { this.removeById(e.id); return; }
    const color = cssVar('--liquidity', '#ffd54f');
    this.upsertRect(e.id, {
      startTime: (e.ts_start / 1000) as Time,
      endTime: (e.ts_end / 1000) as Time,
      priceLow: e.price,
      priceHigh: e.price,
      autoscalePrice: e.price,
      zOrder: 'top',
      style: { fill: color, stroke: color, strokeWidth: e.swept ? 1 : 1.25, dashed: true, visible: true },
    }, e);
    this.markerStructures.set(e.id, e);
    this.scheduleFlushMarkers();
  }

  private upsertLiquidityReversal(r: LiquidityReversal) {
    if (!this.filter.showLiquidityReversal || !this.reversalConfirmEnabled(r)) { this.removeById(r.id); return; }
    const [sweepSourceTs, sweepTs] = this.reversalSweepSegment(r);
    if (!this.isSweepSegmentVisible(sweepSourceTs, sweepTs)) { this.removeById(r.id); return; }
    this.markerStructures.set(r.id, r);
    this.scheduleFlushMarkers();
    this.upsertReversalContext(r);
  }

  private rebuildPo3StageBoxes() {
    const po3s = Array.from(this.allStructures.values())
      .filter((item): item is PowerOf3 => item.kind === 'power_of_3')
      .filter((p) => this.structureMatchesTarget(p));

    const sorted = po3s.sort((a, b) => a.confirm_ts - b.confirm_ts || a.id.localeCompare(b.id));
    for (let index = 0; index < sorted.length; index += 1) {
      const p = sorted[index];
      if (!this.filter.showPo3) {
        this.removeById(p.id);
        continue;
      }
      if (this.filter.showPo3StageBoxes) this.upsertPo3StageBoxes(p, index);
      else this.removePo3StageBoxes(p.id);
      if (this.filter.showPo3Markers && typeof this.markerTimeSecForMs(markerTs(p)) === 'number') {
        const withDisplayIndex = { ...p, display_index: index + 1 } as PowerOf3 & { display_index: number };
        this.markerStructures.set(p.id, withDisplayIndex);
      }
      else {
        this.markerStructures.delete(p.id);
      }
    }
    this.scheduleFlushMarkers();
  }

  private removePo3StageBoxes(id: string) {
    for (const stage of ['accumulation', 'manipulation', 'distribution', 'entry', 'confirm']) {
      const primId = `${id}:${stage}`;
      const entry = this.prims.get(primId);
      if (entry) {
        try { this.series.detachPrimitive(entry.prim); } catch { /* ignore */ }
        this.prims.delete(primId);
      }
    }
  }

  private upsertPo3StageBoxes(p: PowerOf3, groupIndex: number) {
    this.removePo3StageBoxes(p.id);
    const active = new Set<string>();
    let anyBoxRendered = false;
    for (const box of p.stage_boxes ?? []) {
      if (box.stage === 'accumulation' && !this.filter.showPo3AccumulationStage) continue;
      if (box.stage === 'manipulation' && !this.filter.showPo3ManipulationStage) continue;
      if (box.stage === 'distribution' && !this.filter.showPo3DistributionStage) continue;
      const id = `${p.id}:${box.stage}`;
      active.add(id);
      const endMs = Math.max(box.ts_end, box.ts_start + this.barSeconds * 1000);
      const startLogical = this.logicalIndexForMs(box.ts_start);
      const endLogical = this.logicalIndexForMs(endMs);
      if (typeof startLogical !== 'number' || typeof endLogical !== 'number') {
        const existing = this.prims.get(id);
        if (existing) {
          try { this.series.detachPrimitive(existing.prim); } catch { /* ignore */ }
          this.prims.delete(id);
        }
        continue;
      }
      anyBoxRendered = true;
      const style = po3StageStyle(box.stage, p.direction, groupIndex);
      this.upsertRect(id, {
        startTime: (box.ts_start / 1000) as Time,
        endTime: (endMs / 1000) as Time,
        startLogical,
        endLogical,
        priceLow: box.price_low,
        priceHigh: box.price_high,
        autoscalePrice: (box.price_low + box.price_high) / 2,
        zOrder: box.stage === 'manipulation' ? 'top' : 'normal',
        style: { ...style, dashed: box.stage !== 'distribution', strokeWidth: box.stage === 'manipulation' ? 1.5 : 1, visible: true },
        label: po3StageLabel(box.stage, p.direction, p.state, box.label, groupIndex),
      }, p);
    }
    if (anyBoxRendered) {
      this.upsertPo3EntryLine(p, groupIndex);
      this.upsertPo3ConfirmLine(p, groupIndex);
    }
    for (const stage of ['accumulation', 'manipulation', 'distribution']) {
      const id = `${p.id}:${stage}`;
      if (!active.has(id)) {
        const entry = this.prims.get(id);
        if (entry) {
          try { this.series.detachPrimitive(entry.prim); } catch { /* ignore */ }
          this.prims.delete(id);
        }
      }
    }
  }

  private upsertPo3EntryLine(p: PowerOf3, groupIndex: number) {
    const id = `${p.id}:entry`;
    const line = this.po3EntryOrConfirmLine(p);
    if (!line || typeof this.markerTimeSecForMs(line.ts) !== 'number') {
      const existing = this.prims.get(id);
      if (existing) {
        try { this.series.detachPrimitive(existing.prim); } catch { /* ignore */ }
        this.prims.delete(id);
      }
      return;
    }
    const entryTime = this.markerTimeSecForMs(line.ts)!;
    this.upsertTriggerLine(
      id,
      p,
      entryTime as Time,
      line.price,
      structureLineColor(),
      line.kind === 'mss',
      5,
      po3EntryLineLabel(line, groupIndex),
      true,
    );
  }

  private po3EntryOrConfirmLine(p: PowerOf3): Po3SignalLine | null {
    // Entry line (white) only when there is an actual lower-TF entry signal.
    if (typeof p.entry_ts === 'number' && typeof p.entry_price === 'number' && p.entry_kind) {
      return {
        ts: p.entry_ts,
        price: p.entry_price,
        tf: p.entry_tf ?? p.tf,
        kind: p.entry_kind,
      };
    }
    return null;
  }

  private po3ConfirmLine(p: PowerOf3): Po3SignalLine | null {
    // Confirm line (blue) shows for any confirmed PO3, regardless of entry.
    if (p.state !== 'reversal_confirmed' && p.state !== 'distribution_confirmed') {
      return null;
    }
    const confirm = this.allStructures.get(p.confirm_id);
    if (confirm?.kind === 'mss' || confirm?.kind === 'cisd') {
      return { ts: confirm.break_ts, price: confirm.break_price, tf: confirm.tf, kind: confirm.kind };
    }
    if (p.confirm_id) {
      const price = p.direction === 'bullish' ? p.accumulation_high : p.accumulation_low;
      return { ts: p.confirm_ts, price, tf: p.tf, kind: p.confirm_kind };
    }
    return null;
  }

  private upsertPo3ConfirmLine(p: PowerOf3, groupIndex: number) {
    const id = `${p.id}:confirm`;
    const line = this.po3ConfirmLine(p);
    if (!line || typeof this.markerTimeSecForMs(line.ts) !== 'number') {
      const existing = this.prims.get(id);
      if (existing) {
        try { this.series.detachPrimitive(existing.prim); } catch { /* ignore */ }
        this.prims.delete(id);
      }
      return;
    }
    const confirmTime = this.markerTimeSecForMs(line.ts)!;
    this.upsertTriggerLine(
      id,
      p,
      confirmTime as Time,
      line.price,
      po3ConfirmLineColor(),
      line.kind === 'mss',
      5,
      po3EntryLineLabel(line, groupIndex),
      true,
    );
  }

  private reversalSweepSegment(r: LiquidityReversal): [number, number] {
    const sweep = this.allStructures.get(r.sweep_id);
    if (sweep?.kind === 'liquidity_sweep') return [sweep.level_ts, sweep.sweep_ts];
    return [r.sweep_level_ts ?? r.sweep_ts, r.sweep_ts];
  }

  private upsertReversalContext(r: LiquidityReversal) {
    const sweep = this.allStructures.get(r.sweep_id);
    if (sweep?.kind === 'liquidity_sweep') {
      this.upsertRect(`${r.id}:sweep`, {
        startTime: (sweep.level_ts / 1000) as Time,
        endTime: (sweep.sweep_ts / 1000) as Time,
        priceLow: sweep.level_price,
        priceHigh: sweep.level_price,
        autoscalePrice: sweep.level_price,
        endpointDots: 'both',
        zOrder: 'top',
        style: { fill: sweepColor(sweep), stroke: sweepColor(sweep), strokeWidth: 1.5, dashed: true, visible: true },
      }, r);
    } else if (typeof r.sweep_level_ts === 'number') {
      const color = directionColor(r.direction);
      this.upsertRect(`${r.id}:sweep`, {
        startTime: (r.sweep_level_ts / 1000) as Time,
        endTime: (r.sweep_ts / 1000) as Time,
        priceLow: r.level_price,
        priceHigh: r.level_price,
        autoscalePrice: r.level_price,
        endpointDots: 'both',
        zOrder: 'top',
        style: { fill: color, stroke: color, strokeWidth: 1.5, dashed: true, visible: true },
      }, r);
    } else {
      this.removeById(`${r.id}:sweep`);
    }

    const confirm = this.allStructures.get(r.confirm_id);
    if (confirm?.kind === 'mss') {
      this.upsertTriggerLine(`${r.id}:confirm`, r, (confirm.break_ts / 1000) as Time, confirm.swing_price, structureLineColor(), true, 5);
    } else if (confirm?.kind === 'cisd') {
      this.upsertTriggerLine(`${r.id}:confirm`, r, (confirm.break_ts / 1000) as Time, confirm.leg_origin_price, structureLineColor(), false, 5);
    } else if (typeof r.confirm_price === 'number') {
      const dashed = r.confirm_kind === 'mss';
      this.upsertTriggerLine(`${r.id}:confirm`, r, (r.confirm_ts / 1000) as Time, r.confirm_price, structureLineColor(), dashed, 5);
    }
  }

  private reversalConfirmEnabled(r: LiquidityReversal): boolean {
    if (r.confirm_kind === 'cisd') return this.filter.showLiquidityReversalCisd;
    if (r.confirm_kind === 'mss') return this.filter.showLiquidityReversalMss;
    return true;
  }

  private sweepKindEnabled(s: LiquiditySweep): boolean {
    if (s.pool_kind === 'swing_high' || s.pool_kind === 'swing_low') return this.filter.showSwingSweeps;
    if (s.pool_kind === 'equal_highs' || s.pool_kind === 'equal_lows') return this.filter.showEqhEql;
    if (s.pool_kind === 'pdh' || s.pool_kind === 'pdl') return this.filter.showPdhPdlSweeps;
    return true;
  }

  private upsertTriggerLine(
    id: string,
    structure: IctStructure,
    startTime: Time,
    price: number,
    color: string,
    dashed: boolean,
    bars: number,
    label?: string,
    forceLabel = false,
  ) {
    const halfHeight = triggerBandHalfHeight(price);
    const spec: RectSpec = {
      startTime,
      endTime: (Number(startTime) + this.barSeconds * bars) as Time,
      priceLow: forceLabel ? price : price - halfHeight,
      priceHigh: forceLabel ? price : price + halfHeight,
      lineBars: bars,
      zOrder: 'top',
      style: {
        fill: color,
        stroke: undefined,
        strokeWidth: 0,
        dashed,
        visible: true,
      },
      label,
      forceLabel,
    };
    this.upsertRect(id, spec, structure);
  }

  private upsertSmt(s: SmtDivergence) {
    // White sweep evidence and DXY HTF PDA context are persistent chart
    // layers: every current-rule SMT returned by list_structures is painted.
    // Inbox selection is a separate concern and controls chain markers plus
    // the temporary yellow C1-C3/PDA navigation overlays.
    this.smtStructures.set(s.id, s);
    // Re-evaluate all lines because filter/target changes can affect the
    // complete persistent set.
    const eligible = this.smtSweepEligible();
    for (const ss of this.smtStructures.values()) {
      this.upsertSmtSweepLine(ss, eligible);
    }
    this.scheduleFlushMarkers();
    // PDA rectangles are deduplicated by PDA id and shown only on DXY.
    this.rebuildSmtPdaZones();
  }

  /** HTF PDA context zones (sec 5.4). Drawn on the sweeper pane only,
   *  on either the native HTF or the SMT comparison/MTF. One rectangle per
   *  unique PDA id (dedup): many SMTs can share a PDA and stacking
   *  translucent fills would obscure candles. */
  private rebuildSmtPdaZones() {
    const desired = new Map<string, SmtDivergence>();
    if (this.smtEnabled && this.smtHtfPdaEnabled) {
      // Show PDA on: (1) the MTF pane (comparison TF) where the chain
      // plays out, and (2) the HTF pane (context TF) where the PDA
      // natively lives. A dual-role TF (e.g. 1h = MTF for 4h-1h AND HTF
      // for 1h-30m) shows both its MTF-context PDA (from the upper chain)
      // and its native HTF PDA (from the lower chain) - they are distinct
      // zones at different price/time ranges.
      for (const s of this.smtStructures.values()) {
        if (!s.htf_pda_ref) continue;
        // Sweeper pane only (sec 5.4).
        if (this.targetSymbol !== s.sweeper_symbol) continue;
        const isMtf = this.targetTf === s.comparison_timeframe;
        const isHtf = this.targetTf === s.context_timeframe;
        if (!isMtf && !isHtf) continue;
        if (!desired.has(s.htf_pda_ref.id)) desired.set(s.htf_pda_ref.id, s);
      }
    }

    // Persistent zones and the selected-SMT overlay use distinct primitives.
    // The overlay sits on top of the normal purple PDA, so dismissing the
    // selection reveals the unchanged persistent context instead of removing
    // it from the chart.
    const desiredPrimitiveIds = new Set(
      [...desired.keys()].map((pdaId) => `smt_pda:${pdaId}`),
    );
    const highlighted = this.highlightedSmtId
      ? this.smtStructures.get(this.highlightedSmtId)
      : undefined;
    const canHighlightPda = Boolean(
      this.smtEnabled
      && this.smtHtfPdaEnabled
      && highlighted?.htf_pda_ref
      && this.targetSymbol === highlighted.sweeper_symbol
      && this.targetTf === highlighted.comparison_timeframe,
    );
    const highlightPrimitiveId = canHighlightPda && highlighted?.htf_pda_ref
      ? `smt_pda_highlight:${highlighted.id}:${highlighted.htf_pda_ref.id}`
      : null;
    if (highlightPrimitiveId) desiredPrimitiveIds.add(highlightPrimitiveId);

    // Drop zones whose PDA is no longer referenced here, including a prior
    // SMT's yellow selection overlay.
    for (const key of [...this.prims.keys()]) {
      if (!key.startsWith('smt_pda:') && !key.startsWith('smt_pda_highlight:')) continue;
      if (!desiredPrimitiveIds.has(key)) {
        const e = this.prims.get(key);
        if (e) {
          try { this.series.detachPrimitive(e.prim); } catch { /* ignore */ }
          this.prims.delete(key);
        }
      }
    }
    for (const s of desired.values()) {
      const pda = s.htf_pda_ref;
      if (!pda) continue;
      const id = `smt_pda:${pda.id}`;
      // A valid C3 stamps a deterministic consumed edge. Reserved/active
      // PDAs extend to the latest loaded bar instead of ending two candles
      // after confirmation or drifting until an unrelated future price exit.
      const fallbackEndMs = pda.ts_confirm + this.barSeconds * 1000;
      const endMs = pda.exit_ts ?? Math.max(this.lastBarTsMs || 0, fallbackEndMs);
      const openSec = Math.floor(pda.ts_open / 1000);
      const firstSec = this.barTimesSec.length > 0 ? this.barTimesSec[0] : openSec;
      const openSecClamped = openSec < firstSec ? firstSec : openSec;
      const openMsClamped = openSecClamped * 1000;
      const spec: RectSpec = {
        // Clamp PDA start to the first visible bar when ts_open is before
        // the pane data range (HTF PDA on MTF pane). Without this the rect
        // clamps to the viewport left edge (drifts on pan) instead of
        // anchoring to the first bar.
        startTime: openSecClamped as Time,
        endTime: (endMs / 1000) as Time,
        startLogical: this.logicalIndexForMs(openMsClamped),
        endLogical: this.logicalIndexForMs(endMs),
        priceLow: pda.price_low,
        priceHigh: pda.price_high,
        style: {
          fill: 'rgba(179, 136, 255, 0.08)',
          stroke: 'rgba(179, 136, 255, 0.55)',
          strokeWidth: 1,
          dashed: true,
          visible: true,
        },
        label: `HTF PDA ${pda.tf.toUpperCase()} ${pda.direction === 'bullish' ? 'BISI' : 'SIBI'}`,
      };
      this.upsertRect(id, spec, s);
    }

    if (highlightPrimitiveId && highlighted?.htf_pda_ref) {
      const pda = highlighted.htf_pda_ref;
      const fallbackEndMs = pda.ts_confirm + this.barSeconds * 1000;
      const endMs = pda.exit_ts ?? Math.max(this.lastBarTsMs || 0, fallbackEndMs);
      const openSec = Math.floor(pda.ts_open / 1000);
      const firstSec = this.barTimesSec.length > 0 ? this.barTimesSec[0] : openSec;
      const openSecClamped = Math.max(openSec, firstSec);
      const openMsClamped = openSecClamped * 1000;
      this.upsertRect(highlightPrimitiveId, {
        startTime: openSecClamped as Time,
        endTime: (endMs / 1000) as Time,
        startLogical: this.logicalIndexForMs(openMsClamped),
        endLogical: this.logicalIndexForMs(endMs),
        priceLow: pda.price_low,
        priceHigh: pda.price_high,
        zOrder: 'top',
        style: {
          fill: 'rgba(255, 235, 59, 0.14)',
          stroke: 'rgba(255, 235, 59, 0.96)',
          strokeWidth: 2,
          dashed: false,
          visible: true,
        },
      }, highlighted);
    }
  }

 /** Every current-rule SMT applicable to this pane remains eligible for its
   *  white sweep line. An audit-only invalidated row is added only when the
   *  user opens it from the Inbox; pinning keeps that evidence after the
   *  yellow navigation rectangles and PDA overlay are dismissed. */
  private smtSweepEligible(): Set<string> {
    return new Set(
      [...this.smtStructures.values()]
        .filter((s) => this.targetTf === s.comparison_timeframe || this.targetTf === s.context_timeframe)
        .map((s) => s.id),
    );
  }

  private smtSweepEndpoints(s: SmtDivergence): { startMs: number; endMs: number; startPrice: number; endPrice: number; htfLabel: string } | null {
    if (this.targetTf !== s.comparison_timeframe && this.targetTf !== s.context_timeframe) return null;
    // HTF panes share the paired reference/sweep intervals. MTF panes use
    // each symbol's actual sub-candles that contributed those HTF extremes;
    // their exact timestamps are intentionally allowed to differ.
    const sweeperChain = s.chains.find((c) => c.symbol === s.sweeper_symbol);
    if (!sweeperChain) return null;
    const sweeperLiq = s.liquidity_refs.find((l) => l.symbol === s.sweeper_symbol);
    if (!sweeperLiq) return null;
    const liqRef = s.liquidity_refs.find((l) => l.symbol === this.targetSymbol);
    if (!liqRef) return null;
    const targetChain = s.chains.find((c) => c.symbol === this.targetSymbol);
    if (!targetChain) return null;
    const isHtfLayer = this.targetTf === s.context_timeframe;
    if (!isHtfLayer && (!liqRef.mtf_ref_candle || !liqRef.mtf_sweep_candle)) return null;
    const sharedStartMs = isHtfLayer ? s.observation_window[0] : liqRef.mtf_ref_candle!.ts;
    const sharedEndMs = isHtfLayer
      ? s.observation_window[1] - tfToSeconds(s.context_timeframe) * 1000
      : liqRef.mtf_sweep_candle!.ts;
    const startBar = this.barAtOrBefore(sharedStartMs);
    const endBar = this.barAtOrBefore(sharedEndMs);
    if (!startBar || !endBar) return null;
    // `barAtOrBefore` is only a lookup aid, not permission to bridge a data
    // gap. If the requested timestamp is outside that candle's real coverage,
    // omit the line instead of snapping it onto an unrelated older candle.
    const paneBarMs = tfToSeconds(this.targetTf) * 1000;
    if (sharedStartMs >= startBar.timeSec * 1000 + paneBarMs
      || sharedEndMs >= endBar.timeSec * 1000 + paneBarMs) return null;
    if (startBar.timeSec === endBar.timeSec) return null; // degenerate
    const useHigh = liqRef.side === 'buy_side';
    // Endpoint prices are immutable formation evidence. On an HTF pane the
    // line is anchored to this pane's actual HTF candles; using MTF endpoint
    // prices beside snapped HTF timestamps can put the line inside the wick
    // (or even outside the candle) when an old/stale aggregate existed.
    // MTF panes continue to use each symbol's exact contributing child.
    const startPrice = isHtfLayer
      ? (useHigh ? startBar.high : startBar.low)
      : (useHigh ? liqRef.mtf_ref_candle!.high : liqRef.mtf_ref_candle!.low);
    const endPrice = isHtfLayer
      ? (useHigh ? endBar.high : endBar.low)
      : (useHigh ? liqRef.mtf_sweep_candle!.high : liqRef.mtf_sweep_candle!.low);
    // PDA prices are expressed on the sweeper's scale. Verify the actual
    // per-pane endpoints there as a final guard against stale historical
    // geometry; EU/GU are constrained by the shared timestamps instead.
    if (this.targetSymbol === s.sweeper_symbol && s.htf_pda_ref) {
      const { price_low: low, price_high: high } = s.htf_pda_ref;
      if (startPrice < low || startPrice > high || endPrice < low || endPrice > high) return null;
    }
    return {
      // HTF lines must bind to real HTF bar opens. A reference can originate
      // from a local MTF child (for example 14:00 inside an 11:00-15:00 4H
      // bar); passing that child timestamp to lightweight-charts yields no
      // coordinate on the 4H series and can make the line disappear. All
      // three panes share the same canonical HTF grid, so these snapped bar
      // opens remain identical across panes. MTF endpoints stay on each
      // symbol's exact child-candle timestamps.
      startMs: isHtfLayer ? startBar.timeSec * 1000 : sharedStartMs,
      endMs: isHtfLayer ? endBar.timeSec * 1000 : sharedEndMs,
      startPrice,
      endPrice,
      htfLabel: s.context_timeframe.toUpperCase(),
    };
  }

  private upsertSmtSweepLine(s: SmtDivergence, eligible?: Set<string>) {
    const id = `${s.id}:smt_sweep`;
    const isEligible = (eligible ?? this.smtSweepEligible()).has(s.id);
    // Active current-rule SMTs are persistent. A terminal/invalidated row
    // explicitly opened from the Inbox remains auditable through focus.
    if (!this.smtEnabled || !this.smtSweepLineEnabled || !isEligible) {
      this.removeSweepSeries(id);
      return;
    }
    const ep = this.smtSweepEndpoints(s);
    if (!ep) {
      this.removeSweepSeries(id);
      return;
    }
    const startSec = Math.floor(ep.startMs / 1000);
    const endSec = Math.floor(ep.endMs / 1000);
    if (startSec === endSec) {
      this.removeSweepSeries(id);
      return;
    }
    // Single continuous diagonal line via custom primitive (§5.8).
    // The primitive draws the line + a zoom-scaling label at the endpoint.
    const spec: SmtSweepSpec = {
      startTime: startSec as Time,
      endTime: endSec as Time,
      startPrice: ep.startPrice,
      endPrice: ep.endPrice,
      label: `SMT ${ep.htfLabel}`,
      color: '#ffffff',
    };
    const existing = this.smtSweepPrims.get(id);
    if (existing) {
      existing.setSpec(spec);
    } else {
      const prim = new SmtSweepPrimitive(spec);
      try { this.series.attachPrimitive(prim); } catch (e) { console.warn('attach smt sweep prim failed', e); return; }
      this.smtSweepPrims.set(id, prim);
    }
  }

  private removeSweepSeries(id: string) {
    const prim = this.smtSweepPrims.get(id);
    if (prim) {
      try { this.series.detachPrimitive(prim); } catch { /* ignore */ }
      this.smtSweepPrims.delete(id);
    }
  }

  private scheduleFlushMarkers() {
    if (this.markerFlushRaf) return;
    this.markerFlushRaf = requestAnimationFrame(() => {
      this.markerFlushRaf = 0;
      this.flushMarkers();
    });
  }

  private flushMarkers() {
    if (!this.markersApi) return;
    const grouped = new Map<string, MarkerGroup>();
    for (const item of this.markerStructures.values()) {
      const timeSec = this.markerTimeSecForMs(markerTs(item));
      if (typeof timeSec !== 'number') continue;
      const key = `${timeSec}|${markerDirection(item)}`;
      const group = grouped.get(key) ?? { timeSec };
      if (item.kind === 'mss') group.mss = item;
      else if (item.kind === 'cisd') group.cisd = item;
      else if (item.kind === 'bos') group.bos = item;
      else if (item.kind === 'liquidity_reversal') group.reversal = item;
      else if (item.kind === 'power_of_3') group.po3 = item;
      else if (item.kind === 'liquidity_sweep') group.sweep = item;
      else group.eq = item;
      grouped.set(key, group);
    }
    const arr = Array.from(grouped.values()).map((group) => ({ ...this.markerForGroup(group), time: group.timeSec as Time }));
    // Append SMT chain markers (C1/SMT K/C2/C3) - they coexist with
    // CISD/MSS, never suppressed (§5.2).
    arr.push(...this.smtChainMarkers());
    const confirm = this.highlightConfirm;
    if (confirm && confirm.symbol === this.targetSymbol) {
      const time = this.markerTimeSecForMs(confirm.ts);
      if (typeof time === 'number') {
        const bullish = confirm.direction === 'bullish';
        arr.push({
          time: time as Time,
          position: bullish ? 'belowBar' : 'aboveBar',
          shape: bullish ? 'arrowUp' : 'arrowDown',
          color: confirm.label === 'ALERT' ? '#ff9800' : '#ffee58',
          text: `${confirm.label} ${confirm.kind.toUpperCase()}${bullish ? '↑' : '↓'}`,
          size: 3,
        });
      }
    }
    arr.sort((a, b) => Number(a.time) - Number(b.time));
    this.markersApi.setMarkers(arr);
  }

  private markerForGroup(group: MarkerGroup): SeriesMarker<Time> {
    if (group.reversal) return this.reversalMarker(group.reversal);
    if (group.po3) return this.po3Marker(group.po3);
    const primary = group.cisd ?? group.mss;
    if (!primary && group.sweep) return this.sweepMarker(group.sweep);
    if (!primary && group.bos) return this.bosMarker(group.bos);
    if (!primary) return this.eqMarker(group.eq!);
    const isBull = primary.direction === 'bullish';
    const arrow = isBull ? '↑' : '↓';
    const hasBoth = Boolean(group.cisd && group.mss);
    const isCisdFirst = Boolean(group.cisd);
    return {
      time: (primary.break_ts / 1000) as Time,
      position: isBull ? 'belowBar' : 'aboveBar',
      shape: isCisdFirst ? 'circle' : (isBull ? 'arrowUp' : 'arrowDown'),
      color: directionColor(primary.direction),
      text: hasBoth ? `CISD+MSS${arrow}` : isCisdFirst ? `CISD${arrow}` : `MSS${arrow}`,
      size: isCisdFirst ? 1 : 1,
    };
  }

  private bosMarker(b: Bos): SeriesMarker<Time> {
    const isBull = b.direction === 'bullish';
    return {
      time: (b.break_ts / 1000) as Time,
      position: isBull ? 'belowBar' : 'aboveBar',
      shape: 'circle',
      color: directionColor(b.direction),
      text: isBull ? 'BoS↑' : 'BoS↓',
      size: 1,
    };
  }

  private sweepMarker(s: LiquiditySweep): SeriesMarker<Time> {
    const buySide = s.side === 'buy_side';
    return {
      time: (s.sweep_ts / 1000) as Time,
      position: buySide ? 'aboveBar' : 'belowBar',
      shape: 'circle',
      color: sweepColor(s),
      text: sweepLabel(s),
      size: 1,
    };
  }

  private eqMarker(e: EqualHighsLows): SeriesMarker<Time> {
    const buySide = e.side === 'buy_side';
    return {
      time: (e.ts_end / 1000) as Time,
      position: buySide ? 'aboveBar' : 'belowBar',
      shape: 'circle',
      color: cssVar('--liquidity', '#ffd54f'),
      text: buySide ? 'EQH' : 'EQL',
      size: 1,
    };
  }

  private smtChainMarkers(): SeriesMarker<Time>[] {
    if (!this.smtEnabled || !this.smtChainEnabled) return [];
    const out: SeriesMarker<Time>[] = [];
    const color = cssVar('--smt-trigger', '#b388ff');
    for (const s of this.smtStructures.values()) {
      // C1/SMT K/C2/C3 are selection details rather than the persistent
      // background evidence layer. Keep them bound to the currently pinned
      // Inbox row so the default HTF/MTF views only show white lines + PDA.
      if (!this.isFocusedSmt(s.id)) continue;
      // Find the chain for this pane symbol (§2.2: each symbol has its own chain)
      const chain = s.chains.find((c) => c.symbol === this.targetSymbol);
      if (!chain) continue;
      if (chain.detection_state === 'invalidated' && !this.isFocusedSmt(s.id)) continue;
      // C1/SMT K/C2/C3 markers only on the MTF pane (where the chain lives)
      if (this.targetTf !== s.comparison_timeframe) continue;
      // Flip direction for inverse-correlated counters (EU/GU) so their
      // arrows point opposite to DXY: DXY bullish (swept low) = EU/GU
      // bearish (expected to drop). The white sweep line already does
      // this via per-symbol liqRef.side; chain markers need the same.
      const isSweeper = this.targetSymbol === s.sweeper_symbol;
      const baseBull = s.candidate_direction === 'bullish';
      const isBull = isSweeper
        ? baseBull
        : s.relationship === 'negative' ? !baseBull : baseBull;
      const pos: 'aboveBar' | 'belowBar' = isBull ? 'belowBar' : 'aboveBar';

     // C1 is part of the persisted chain and defines the left edge used by
     // every C2 case. It was previously omitted even though the UI claimed to
     // show C1/SMT K/C2/C3, making a corrected chain look unchanged.
     const c1Time = this.markerTimeSecForMs(chain.c1_candle.ts);
     if (typeof c1Time === 'number') {
       out.push({
         time: c1Time as Time,
         position: pos,
         shape: 'circle',
         color: 'rgba(255,235,59,0.82)',
         text: 'C1',
         size: 1,
       });
     }

     // SMT K marker (core divergence bar)
     const kTime = this.markerTimeSecForMs(chain.smt_k_candle.ts);
     if (typeof kTime === 'number') {
       out.push({
         time: kTime as Time,
         position: pos,
          shape: isBull ? 'arrowUp' : 'arrowDown',
         color,
         text: 'K',
         size: 1,
       });
     }

     // C2 marker (confirmation bar)
      const c2Color = 'rgba(179,136,255,0.7)';
     if (chain.c2_candle) {
       const c2Time = this.markerTimeSecForMs(chain.c2_candle.ts);
       if (typeof c2Time === 'number') {
         out.push({
           time: c2Time as Time,
           position: pos,
           shape: 'circle',
           color: c2Color,
            text: 'C2',
           size: 1,
         });
       }
     }

     // C3 marker (entry bar)
      const c3Color = 'rgba(179,136,255,0.45)';
     if (chain.c3_candle) {
       const c3Time = this.markerTimeSecForMs(chain.c3_candle.ts);
       if (typeof c3Time === 'number') {
         const c3Failed = chain.detection_state === 'invalidated';
         out.push({
           time: c3Time as Time,
           position: pos,
           shape: 'circle',
           color: c3Failed ? '#ef5350' : c3Color,
            text: c3Failed ? 'C3失败' : 'C3',
           size: 1,
         });
       }
     }

    }
    return out;
  }


  private reversalMarker(r: LiquidityReversal): SeriesMarker<Time> {
    const isBull = r.direction === 'bullish';
    const arrow = isBull ? '↑' : '↓';
    const star = r.score >= 4 ? '★' : '';
    const sweep = this.allStructures.get(r.sweep_id);
    const prefix = sweep?.kind === 'liquidity_sweep' ? sweepPoolLabel(sweep) : reversalSweepPoolLabel(r);
    return {
      time: (r.confirm_ts / 1000) as Time,
      position: isBull ? 'belowBar' : 'aboveBar',
      shape: isBull ? 'arrowUp' : 'arrowDown',
      color: directionColor(r.direction),
      text: `${prefix}+${r.confirm_kind.toUpperCase()}${arrow}${star}`,
      size: 2,
    };
  }

  private po3Marker(p: PowerOf3): SeriesMarker<Time> {
    const isBull = p.direction === 'bullish';
    const arrow = isBull ? '↑' : '↓';
    const displayIndex = (p as PowerOf3 & { display_index?: number }).display_index;
    const prefix = typeof displayIndex === 'number' ? `P${displayIndex} ` : '';
    const stateText = p.state === 'distribution_confirmed'
      ? `${prefix}PO3✓${arrow}`
      : `${prefix}PO3${arrow}`;
    return {
      time: (markerTs(p) / 1000) as Time,
      position: isBull ? 'belowBar' : 'aboveBar',
      shape: isBull ? 'arrowUp' : 'arrowDown',
      color: directionColor(p.direction),
      text: stateText,
      size: 2,
    };
  }

  private repaintAll() {
    // Detach all chart-side artifacts; the authoritative cache is
    // `allStructures`, which we re-emit through upsertStructure so the
    // current filter decides what stays visible.
    for (const { prim } of this.prims.values()) {
      try { this.series.detachPrimitive(prim); } catch { /* ignore */ }
    }
    this.prims.clear();
    for (const { line } of this.priceLines.values()) {
      try { this.series.removePriceLine(line); } catch { /* ignore */ }
    }
    for (const { autoscalePrim } of this.priceLines.values()) {
      try { this.series.detachPrimitive(autoscalePrim); } catch { /* ignore */ }
    }
    this.priceLines.clear();
    this.markerStructures.clear();
    this.smtStructures.clear();
    for (const prim of this.smtSweepPrims.values()) {
      try { this.series.detachPrimitive(prim); } catch { /* ignore */ }
    }
    this.smtSweepPrims.clear();
    this.scheduleFlushMarkers();
    this.rebuildPo3StageBoxes();
    for (const item of this.allStructures.values()) {
      if (item.kind === 'power_of_3') continue;
      if (!this.structureMatchesTarget(item)) {
        this.removeById(item.id);
        continue;
      }
      this.upsertStructure(item);
    }
  }

  /** Devtools / log-forwarder snapshot. Writes the current internal
   *  state to console (which uiLog mirrors to /tmp/ict-radar-ui.log)
   *  so we can correlate Rust-side detector emit logs with the
   *  front-end's view of those structures during M3 debugging. */
  dump(tag: string = 'snapshot'): void {
    const allArr = Array.from(this.allStructures.values()).map((s) => ({
      id: s.id, kind: (s as { kind: string }).kind,
      symbol: 'sweeper_symbol' in s ? s.sweeper_symbol : ('symbol' in s ? s.symbol : ''), tf: 'comparison_timeframe' in s ? s.comparison_timeframe : ('tf' in s ? s.tf : ''),
      ...((s as { state?: string }).state ? { state: (s as { state?: string }).state } : {}),
      ...(typeof (s as { price?: number }).price === 'number' ? { price: (s as { price?: number }).price } : {}),
    }));
    const primsArr = Array.from(this.prims.keys());
    const linesArr = Array.from(this.priceLines.entries()).map(([id, v]) => ({ id, price: v.level.price, label: v.level.label }));
    const markersArr = Array.from(this.markerStructures.keys());
    console.log('[ovl.dump]', tag, {
      target: { symbol: this.targetSymbol, tf: this.targetTf },
      filter: this.filter,
      lastBarTsMs: this.lastBarTsMs,
      allStructures: allArr,
      prims: primsArr,
      priceLines: linesArr,
      markers: markersArr,
    });
  }

  dispose() {
    if (this.markerFlushRaf) cancelAnimationFrame(this.markerFlushRaf);
    this.markerFlushRaf = 0;
    this.clear();
  }
}

// Per-TF "visible structure window": only keep FVG/OB/MSS/CISD whose
// anchor timestamp falls within the most recent N bars on the active
// chart. Without this guard, every backend structure ever seen by
// SQLite (4881 FVG + 3780 OB observed in user's log) gets attached
// to the chart, so primitives whose ts_open pre-dates the visible
// range render as "long bars from the left edge" and markers pile up
// at the leftmost visible bar. Numbers are picked so each TF has
// roughly the same visible structure density (~bands per screen).
function tfVisibleBars(tf: string): number {
  switch (tf) {
    case '1m': return 300;
    case '5m': return 300;
    case '15m': return 300;
    case '30m': return 200;
    case '1h': return 200;
    case '4h': return 150;
    case '1d': return 120;
    case '1w': return 80;
    default: return 200;
  }
}

function reversalSweepPoolLabel(r: LiquidityReversal): string {
  switch (r.sweep_pool_kind) {
    case 'swing_high': return 'BSL';
    case 'swing_low': return 'SSL';
    case 'equal_highs': return 'EQH';
    case 'equal_lows': return 'EQL';
    case 'pdh': return 'PDH';
    case 'pdl': return 'PDL';
    default: return 'Sweep';
  }
}

function po3EntryLineLabel(line: Po3SignalLine, groupIndex: number): string {
  const tf = line.tf;
  const kind = line.kind.toUpperCase();
  return `P${groupIndex + 1} ${tf}_${kind}`;
}

function tfToSeconds(tf: string): number {
  switch (tf) {
    case '1m': return 60;
    case '5m': return 5 * 60;
    case '15m': return 15 * 60;
    case '30m': return 30 * 60;
    case '1h': return 60 * 60;
    case '4h': return 4 * 60 * 60;
    case '1d': return 24 * 60 * 60;
    case '1w': return 7 * 24 * 60 * 60;
    default: return 60;
  }
}

function markerTs(item: MarkerStructure): number {
  if (item.kind === 'mss' || item.kind === 'cisd' || item.kind === 'bos') return item.break_ts;
  if (item.kind === 'liquidity_reversal') return item.confirm_ts;
  if (item.kind === 'equal_highs_lows') return item.ts_end;
  if (item.kind === 'power_of_3') return item.entry_ts ?? item.sweep_ts;
  return item.sweep_ts;
}

function markerDirection(item: MarkerStructure): 'bullish' | 'bearish' {
  if (item.kind === 'liquidity_sweep') return item.side === 'buy_side' ? 'bearish' : 'bullish';
  if (item.kind === 'equal_highs_lows') return item.side === 'buy_side' ? 'bearish' : 'bullish';
  return item.direction;
}

function sweepLabel(s: LiquiditySweep): string {
  switch (s.pool_kind) {
    case 'swing_high': return 'BSL ×';
    case 'swing_low': return 'SSL ×';
    case 'equal_highs': return 'EQH sweep ×';
    case 'equal_lows': return 'EQL sweep ×';
    case 'pdh': return 'PDH sweep ×';
    case 'pdl': return 'PDL sweep ×';
  }
}

function sweepPoolLabel(s: LiquiditySweep): string {
  switch (s.pool_kind) {
    case 'swing_high': return 'BSL';
    case 'swing_low': return 'SSL';
    case 'equal_highs': return 'EQH';
    case 'equal_lows': return 'EQL';
    case 'pdh': return 'PDH';
    case 'pdl': return 'PDL';
  }
}

function sweepColor(s: LiquiditySweep): string {
  switch (s.pool_kind) {
    case 'swing_high':
    case 'swing_low':
      return '#4fc3ff'; // bright cyan-blue: BSL / SSL swing sweep
    case 'equal_highs':
    case 'equal_lows':
      return '#ffd54f'; // yellow: EQH / EQL sweep
    case 'pdh':
    case 'pdl':
      return '#ec407a'; // magenta: PDH / PDL sweep
  }
}

function sweepPriority(s: LiquiditySweep): number {
  switch (s.pool_kind) {
    case 'pdh':
    case 'pdl':
      return 3;
    case 'equal_highs':
    case 'equal_lows':
      return 2;
    case 'swing_high':
    case 'swing_low':
      return 1;
  }
}

function pdSideLabel(side: PremiumDiscount['current_side']): string {
  switch (side) {
    case 'premium': return 'Premium';
    case 'discount': return 'Discount';
    case 'equilibrium': return 'EQ';
  }
}

function sessionShortLabel(session: SessionRange['session']): string {
  switch (session) {
    case 'asia': return 'Asia';
    case 'london_open': return 'LO';
    case 'new_york_open': return 'NY';
    case 'london_close': return 'LC';
  }
}

function sessionBoxLabel(session: SessionRange['session']): string {
  switch (session) {
    case 'asia': return 'Asia';
    case 'london_open': return 'London';
    case 'new_york_open': return 'NY';
    case 'london_close': return 'LC';
  }
}

function po3StageStyle(stage: PowerOf3['stage_boxes'][number]['stage'], direction: PowerOf3['direction'], groupIndex: number) {
  const alphaStep = groupIndex % 4;
  if (stage === 'accumulation') {
    return {
      fill: `rgba(0, 188, 212, ${0.06 + alphaStep * 0.015})`,
      stroke: PO3_STAGE_BOX.accumulation.stroke,
    };
  }
  if (stage === 'manipulation') {
    return {
      fill: `rgba(255, 112, 67, ${0.08 + alphaStep * 0.018})`,
      stroke: PO3_STAGE_BOX.manipulation.stroke,
    };
  }
  const dist = direction === 'bullish' ? PO3_STAGE_BOX.distributionBull : PO3_STAGE_BOX.distributionBear;
  const rgb = direction === 'bullish' ? '38, 166, 154' : '239, 83, 80';
  return {
    fill: `rgba(${rgb}, ${0.06 + alphaStep * 0.015})`,
    stroke: dist.stroke,
  };
}

function po3StageLabel(
  stage: PowerOf3['stage_boxes'][number]['stage'],
  direction: PowerOf3['direction'],
  state: PowerOf3['state'],
  label: string | undefined,
  groupIndex: number,
): string {
  const arrow = direction === 'bullish' ? '↑' : '↓';
  const group = `P${groupIndex + 1}`;
  const base = label ?? (stage === 'accumulation'
    ? 'PO3 Acc'
    : stage === 'manipulation'
      ? 'PO3 Manip'
      : state === 'distribution_confirmed'
        ? `PO3✓ Dist${arrow}`
        : `PO3 Dist${arrow}`);
  return `${group} ${base}`;
}

function sameSweepCluster(a: LiquiditySweep, b: LiquiditySweep): boolean {
  if (a.side !== b.side) return false;
  if (a.level_ts !== b.level_ts || a.sweep_ts !== b.sweep_ts) return false;
  const tolerance = liquidityDedupeTolerance(Math.max(Math.abs(a.level_price), Math.abs(b.level_price)));
  return Math.abs(a.level_price - b.level_price) <= tolerance;
}

function liquidityDedupeTolerance(price: number): number {
  return price > 10 ? 0.05 : 0.00005;
}
