// Lightweight-charts v5 series primitive: a price/time rectangle.
// Used for FVG / OB shading and invisible autoscale helpers.

import type {
  IChartApi,
  ISeriesApi,
  ISeriesPrimitive,
  IPrimitivePaneRenderer,
  IPrimitivePaneView,
  PrimitivePaneViewZOrder,
  SeriesAttachedParameter,
  SeriesType,
  Time,
} from 'lightweight-charts';

type PrimitiveDrawTarget = Parameters<IPrimitivePaneRenderer['draw']>[0];

const _stats: Record<string, number> = Object.create(null);
function bumpStat(k: string) {
  _stats[k] = (_stats[k] ?? 0) + 1;
}
(window as unknown as { __rectStats?: () => Record<string, number> }).__rectStats = () => ({ ..._stats });

const MIN_TRIGGER_LINE_PX = 12;


/** Convert any lwc Time value (number seconds | string ISO | business day)
 *  into integer seconds for comparison with timeScale.getVisibleRange().*/
function toSecondsForCmp(t: Time): number {
  if (typeof t === 'number') return t;
  if (typeof t === 'string') return Math.floor(new Date(t).getTime() / 1000);
  const bd = t as { year: number; month: number; day: number };
  return Math.floor(new Date(bd.year, bd.month - 1, bd.day).getTime() / 1000);
}

export type RectStyle = {
  fill: string;
  stroke?: string;
  /** Border width in CSS pixels. Defaults to 1. Used by IFVG to draw a
   *  visibly thicker frame so 'inverted' is clear at a glance. */
  strokeWidth?: number;
  dashed?: boolean;
  visible?: boolean;
};

export type RectSpec = {
  startTime: Time;
  endTime: Time;
  startLogical?: number;
  endLogical?: number;
  priceLow: number;
  priceHigh: number;
  style: RectStyle;
  zOrder?: PrimitivePaneViewZOrder;
  /** Optional autoscale contribution. Used for price-line-only levels
   *  (PDH/PDL) because lightweight-charts `createPriceLine` itself does
   *  not expand the series price scale when the level is outside the
   *  visible candle range. */
  autoscalePrice?: number;
  /** For priceLow === priceHigh trigger lines, draw exactly this many
   *  bars to the right from startTime. This avoids relying on
   *  timeToCoordinate(endTime) for future timestamps that don't exist in
   *  the series yet. */
  lineBars?: number;
  /** Endpoint dots for line-mode primitives. Defaults to start-only for
   *  MSS/CISD trigger lines; liquidity sweep links use both endpoints to
   *  show level-source bar → sweep bar. */
  endpointDots?: 'start' | 'both' | 'none';
  /** Fill the whole pane vertically. Used for pure time-window backgrounds
   *  such as KillZoneWindow so they never affect price-axis autoscale. */
  fullHeight?: boolean;
  label?: string;
  forceLabel?: boolean;
};

class RectPaneRenderer implements IPrimitivePaneRenderer {
  spec: RectSpec;
  toX: (t: Time) => number | null;
  toLogicalX: (logical: number) => number | null;
  toY: (p: number) => number | null;
  chartHeight: () => number;
  /** Returns the X-pixel range of the currently visible time window.
   *  Used to clamp rect endpoints that fall outside the visible logical
   *  range (otherwise lwc returns null and we'd skip the draw entirely,
   *  which made historical FVG/OB and long-leg CISD lines invisible). */
  visibleEdges: () => { left: number; right: number; leftSec: number; rightSec: number; leftLogical: number; rightLogical: number; barSpacing: number } | null;

  constructor(
    spec: RectSpec,
    toX: (t: Time) => number | null,
    toLogicalX: (logical: number) => number | null,
    toY: (p: number) => number | null,
    chartHeight: () => number,
    visibleEdges: () => { left: number; right: number; leftSec: number; rightSec: number; leftLogical: number; rightLogical: number; barSpacing: number } | null,
  ) {
    this.spec = spec;
    this.toX = toX;
    this.toLogicalX = toLogicalX;
    this.toY = toY;
    this.chartHeight = chartHeight;
    this.visibleEdges = visibleEdges;
  }

  draw(target: PrimitiveDrawTarget) {
    bumpStat('drawCalled');
    target.useBitmapCoordinateSpace((scope: {
      context: CanvasRenderingContext2D;
      horizontalPixelRatio: number;
      verticalPixelRatio: number;
    }) => {
      const { spec } = this;
      if (spec.style.visible === false) { bumpStat('skip:invisible'); return; }
      // ------------------------------------------------------------
      // X-coordinate resolution.
      // ------------------------------------------------------------
      // toX(t) returns null when t is outside the chart's visible
      // logical range. Clamp regular FVG/OB rectangles to visible edges.
      const segmentBars = spec.lineBars;
      const segmentMode = typeof segmentBars === 'number';
      // A rectangle must use one coordinate system for both endpoints. During
      // a live-bar transition a newly-confirmed FVG/OB can briefly have only
      // one endpoint in the pane's bar-index map. Mixing that logical X with
      // a timestamp X made the unresolved end clamp to the opposite pane
      // edge, producing a reversed, almost full-width rectangle.
      const hasLogicalRange = !segmentMode
        && typeof spec.startLogical === 'number'
        && typeof spec.endLogical === 'number';
      let x1: number | null = hasLogicalRange
        ? this.toLogicalX(spec.startLogical!)
        : this.toX(spec.startTime);
      let x2: number | null = hasLogicalRange
        ? this.toLogicalX(spec.endLogical!)
        : this.toX(spec.endTime);
      const edges = this.visibleEdges();
      // No edges = timeScale hasn't laid out yet. Skip; canvas will
      // re-render once it does.
      if (!edges) { bumpStat('skip:noEdges'); return; }
      const startSec = toSecondsForCmp(spec.startTime);
      const endSec = toSecondsForCmp(spec.endTime);
      const fullyLeft = hasLogicalRange ? spec.endLogical! < edges.leftLogical : endSec < edges.leftSec;
      const fullyRight = hasLogicalRange ? spec.startLogical! > edges.rightLogical : startSec > edges.rightSec;
      const xFromVisibleTime = (sec: number): number => {
        const visibleSpanSec = Math.max(1, edges.rightSec - edges.leftSec);
        const ratio = (sec - edges.leftSec) / visibleSpanSec;
        const x = edges.left + (edges.right - edges.left) * ratio;
        return Math.max(edges.left, Math.min(edges.right, x));
      };
      const startInsideVisibleRange = hasLogicalRange
        ? spec.startLogical! > edges.leftLogical && spec.startLogical! < edges.rightLogical
        : startSec > edges.leftSec && startSec < edges.rightSec;
      const endInsideVisibleRange = hasLogicalRange
        ? spec.endLogical! > edges.leftLogical && spec.endLogical! < edges.rightLogical
        : endSec > edges.leftSec && endSec < edges.rightSec;
      if (!segmentMode && startInsideVisibleRange && x1 === null) {
        bumpStat('skip:startCoordPending');
        return;
      }
      if (!segmentMode && endInsideVisibleRange && x2 === null) {
        bumpStat('skip:endCoordPending');
        return;
      }
      if (segmentMode) {
        if (fullyLeft || fullyRight) {
          bumpStat('skip:segmentOffscreen');
          return;
        }
        if (x1 === null) {
          // Don't draw trigger lines whose start is off-screen to the left;
          // they'll appear clamped to the left edge and then jump when
          // the real bar scrolls into view.
          if (startSec < edges.leftSec) {
            bumpStat('skip:segmentStartOffscreenLeft');
            return;
          }
          x1 = xFromVisibleTime(startSec);
        }
      }
      if (fullyLeft || fullyRight) {
        bumpStat('clamp:bothOff');
        if (segmentMode) {
          if (fullyLeft) { x1 = edges.left; x2 = edges.left; }
          else { x1 = edges.right; x2 = edges.right; }
        } else {
          return;
        }
      }
      if (x1 === null) {
        const startBeforeLeft = hasLogicalRange ? spec.startLogical! <= edges.leftLogical : startSec <= edges.leftSec;
        const startAfterRight = hasLogicalRange ? spec.startLogical! >= edges.rightLogical : startSec >= edges.rightSec;
        if (startBeforeLeft) x1 = edges.left;
        else if (startAfterRight) x1 = edges.right;
        else { bumpStat('skip:startCoordPending'); return; }
      }
      if (!segmentMode && startInsideVisibleRange) {
        const expectedStartX = xFromVisibleTime(startSec);
        const minExpectedDistance = Math.max(edges.barSpacing * 1.5, 8);
        const staleLeftTolerance = Math.max(edges.barSpacing, 12);
        if (expectedStartX > edges.left + minExpectedDistance && x1 <= edges.left + staleLeftTolerance) {
          bumpStat('skip:startCoordStaleLeft');
          return;
        }
      }
      if (x2 === null) {
        const endBeforeLeft = hasLogicalRange ? spec.endLogical! <= edges.leftLogical : endSec <= edges.leftSec;
        const endAfterRight = hasLogicalRange ? spec.endLogical! >= edges.rightLogical : endSec >= edges.rightSec;
        if (endBeforeLeft) x2 = edges.left;
        else if (endAfterRight) x2 = edges.right;
        else x2 = xFromVisibleTime(endSec);
      }
      const lineMode = spec.priceLow === spec.priceHigh;
      if (segmentMode) {
        x2 = x1 + Math.max(edges.barSpacing * segmentBars, MIN_TRIGGER_LINE_PX);
      }
      if (x1 === null || x2 === null) { bumpStat('skip:noXCoord'); return; }
      const startClippedLeft = hasLogicalRange
        ? spec.startLogical! < edges.leftLogical
        : startSec < edges.leftSec;
      const endClippedRight = hasLogicalRange
        ? spec.endLogical! > edges.rightLogical
        : endSec > edges.rightSec;

      let y1: number;
      let y2: number;
      if (spec.fullHeight) {
        y1 = 0;
        y2 = this.chartHeight();
      } else {
        const a = this.toY(spec.priceHigh);
        const b = this.toY(spec.priceLow);
        if (a === null || b === null) { bumpStat('skip:noPriceCoord'); return; }
        y1 = Math.min(a, b);
        y2 = Math.max(a, b);
      }
      const left = Math.min(x1, x2) * scope.horizontalPixelRatio;
      const right = Math.max(x1, x2) * scope.horizontalPixelRatio;
      if (!lineMode && right - left < Math.max(2, 2 * scope.horizontalPixelRatio)) {
        bumpStat('skip:rectTooNarrow');
        return;
      }
      const top = y1 * scope.verticalPixelRatio;
      const bottom = y2 * scope.verticalPixelRatio;
      const ctx = scope.context;
      ctx.save();
      if (lineMode) {
        const y = Math.round(top) + 0.5;
        ctx.strokeStyle = spec.style.stroke || spec.style.fill;
        const widthCss = spec.style.strokeWidth ?? 1;
        ctx.lineWidth = Math.max(1, widthCss * scope.horizontalPixelRatio);
        if (spec.style.dashed) {
          ctx.setLineDash([6 * scope.horizontalPixelRatio, 4 * scope.horizontalPixelRatio]);
        }
        ctx.beginPath();
        ctx.moveTo(left, y);
        ctx.lineTo(right, y);
        ctx.stroke();
        const endpointDots = spec.endpointDots ?? 'start';
        if (endpointDots !== 'none') {
          ctx.fillStyle = spec.style.stroke || spec.style.fill;
          const radius = Math.max(2.5, 2.5 * scope.horizontalPixelRatio);
          ctx.beginPath();
          ctx.arc(left, y, radius, 0, Math.PI * 2);
          ctx.fill();
          if (endpointDots === 'both') {
            ctx.beginPath();
            ctx.arc(right, y, radius, 0, Math.PI * 2);
            ctx.fill();
          }
        }
        if (spec.label && (spec.forceLabel || right - left > 28 * scope.horizontalPixelRatio)) {
          ctx.fillStyle = spec.style.stroke || spec.style.fill;
          ctx.font = `${11 * scope.verticalPixelRatio}px sans-serif`;
          ctx.textBaseline = 'bottom';
          ctx.fillText(spec.label, left + 4 * scope.horizontalPixelRatio, y - 2 * scope.verticalPixelRatio);
        }
      } else {
        ctx.fillStyle = spec.style.fill;
        const rawHeight = bottom - top;
        const rectHeight = typeof spec.lineBars === 'number'
          ? Math.max(1, scope.verticalPixelRatio)
          : Math.max(rawHeight, 4 * scope.verticalPixelRatio);
        const adjustedTop = rawHeight < rectHeight ? top - (rectHeight - rawHeight) / 2 : top;
        ctx.fillRect(left, adjustedTop, right - left, rectHeight);
        if (spec.style.stroke) {
          ctx.strokeStyle = spec.style.stroke;
          const widthCss = spec.style.strokeWidth ?? 1;
          ctx.lineWidth = Math.max(1, widthCss * scope.horizontalPixelRatio);
          if (spec.style.dashed) {
            ctx.setLineDash([6 * scope.horizontalPixelRatio, 4 * scope.horizontalPixelRatio]);
          }
          const bottomY = adjustedTop + rectHeight;
          ctx.beginPath();
          ctx.moveTo(left, adjustedTop);
          ctx.lineTo(right, adjustedTop);
          ctx.moveTo(left, bottomY);
          ctx.lineTo(right, bottomY);
          if (!startClippedLeft) {
            ctx.moveTo(left, adjustedTop);
            ctx.lineTo(left, bottomY);
          }
          if (!endClippedRight) {
            ctx.moveTo(right, adjustedTop);
            ctx.lineTo(right, bottomY);
          }
          ctx.stroke();
        }
        if (spec.label && (spec.forceLabel || right - left > 28 * scope.horizontalPixelRatio)) {
          ctx.fillStyle = spec.style.stroke || 'rgba(255,255,255,0.72)';
          ctx.font = `${11 * scope.verticalPixelRatio}px sans-serif`;
          ctx.textBaseline = 'top';
          ctx.fillText(spec.label, left + 4 * scope.horizontalPixelRatio, adjustedTop + 2 * scope.verticalPixelRatio);
        }
      }
      ctx.restore();
    });
  }
}

class RectPaneView implements IPrimitivePaneView {
  private chart: IChartApi;
  private series: ISeriesApi<SeriesType, Time>;
  spec: RectSpec;
  private rendererInstance: RectPaneRenderer;
  /** Subscription handle so we can unsubscribe in `dispose()`; lwc has
   *  no built-in pane-view detach hook so the parent primitive's
   *  `detached()` cleans this up. */
  private rangeUnsub: (() => void) | null = null;
  /** Provided by the parent primitive at construction. Calling this
   *  asks lwc to schedule a chart redraw — without it, mutating the
   *  cached renderer (e.g. on visible-range change) only takes effect
   *  on the *next* unrelated repaint, which produced the
   *  "rectangles vanish after pan/zoom" bug. */
  private requestUpdate: () => void;

  constructor(
    chart: IChartApi,
    series: ISeriesApi<SeriesType, Time>,
    spec: RectSpec,
    requestUpdate: () => void,
  ) {
    this.chart = chart;
    this.series = series;
    this.spec = spec;
    this.requestUpdate = requestUpdate;
    this.rendererInstance = this.makeRenderer();
    // Re-build the renderer whenever the visible time range changes.
    // The renderer caches `toX` resolved against the current scale;
    // without invalidation, when the user pans far enough that a rect's
    // start/end logical positions move from "outside" → "inside" the
    // visible window, the cached coordinates remain null and the rect
    // disappears even though it should now be drawable.
    const ts = chart.timeScale();
    const onRange = () => {
      this.rendererInstance = this.makeRenderer();
      // Ask lwc to schedule a redraw NOW; otherwise the freshly built
      // renderer just sits there until some other event triggers a
      // repaint. Empirically this is what made FVG/OB rectangles
      // disappear after horizontal zoom or right-pan.
      this.requestUpdate();
    };
    ts.subscribeVisibleTimeRangeChange(onRange);
    // Also redraw on logical-range changes (zoom in/out, fitContent,
    // and intra-bar pans that don't shift the time window enough to
    // fire visibleTimeRangeChange but still relocate every bar's X).
    const tsLogical = chart.timeScale();
    tsLogical.subscribeVisibleLogicalRangeChange(onRange);
    this.rangeUnsub = () => {
      ts.unsubscribeVisibleTimeRangeChange(onRange);
      tsLogical.unsubscribeVisibleLogicalRangeChange(onRange);
    };
  }
  dispose() {
    if (this.rangeUnsub) { this.rangeUnsub(); this.rangeUnsub = null; }
  }
  zOrder(): PrimitivePaneViewZOrder {
    return this.spec.zOrder ?? 'normal';
  }
  private makeRenderer() {
    const ts = this.chart.timeScale();
    return new RectPaneRenderer(
      this.spec,
      (t) => ts.timeToCoordinate(t),
      (logical) => ts.logicalToCoordinate(logical as never),
      (p) => this.series.priceToCoordinate(p),
      () => {
        const el = (this.chart as unknown as { chartElement?: () => HTMLElement }).chartElement?.();
        return el?.clientHeight ?? 0;
      },
      () => {
        // Fallback edges for rects whose start/end falls outside the
        // visible logical range. We resolve the visible window's bar
        // range -> screen X via `logicalToCoordinate`, and also expose
        // the corresponding seconds for cull comparisons.
        const range = ts.getVisibleLogicalRange();
        const timeRange = ts.getVisibleRange();
        if (!range || !timeRange) return null;
        const left = ts.logicalToCoordinate(range.from as never);
        const right = ts.logicalToCoordinate(range.to as never);
        if (left === null || right === null) return null;
        return {
          left,
          right,
          leftSec: Number(timeRange.from),
          rightSec: Number(timeRange.to),
          leftLogical: Number(range.from),
          rightLogical: Number(range.to),
          barSpacing: Math.abs(right - left) / Math.max(1, Math.abs(Number(range.to) - Number(range.from))),
        };
      },
    );
  }
  refresh(spec: RectSpec, requestUpdate = true) {
    this.spec = spec;
    this.rendererInstance = this.makeRenderer();
    if (requestUpdate) this.requestUpdate();
  }
  renderer(): IPrimitivePaneRenderer {
    return this.rendererInstance;
  }
}

export class RectanglePrimitive implements ISeriesPrimitive<Time> {
  spec: RectSpec;
  private view: RectPaneView | null = null;
  private chart: IChartApi | null = null;
  private requestUpdate: () => void = () => {};

  constructor(spec: RectSpec) {
    this.spec = spec;
    bumpStat('constructed');
  }

  attached(param: SeriesAttachedParameter<Time, SeriesType>) {
    this.chart = param.chart;
    this.requestUpdate = param.requestUpdate;
    this.view = new RectPaneView(param.chart, param.series, this.spec, param.requestUpdate);
    bumpStat('attached');
  }
  detached() {
    this.view?.dispose();
    this.view = null;
    this.chart = null;
    this.requestUpdate = () => {};
    bumpStat('detached');
  }
  updateAllViews() {
    this.view?.refresh(this.spec, false);
    bumpStat('updateAllViews');
  }
  paneViews(): readonly IPrimitivePaneView[] {
    return this.view ? [this.view] : [];
  }
  autoscaleInfo() {
    if (typeof this.spec.autoscalePrice !== 'number') return null;
    // A primitive must only widen the price axis while its own time span is
    // visible. Previously every cached PDH/PDL/session/sweep level across the
    // entire history contributed to autoscale, so historical SMT navigation
    // compressed the candles into a thin horizontal strip.
    const visible = this.chart?.timeScale().getVisibleRange();
    if (visible) {
      const startSec = toSecondsForCmp(this.spec.startTime);
      const endSec = toSecondsForCmp(this.spec.endTime);
      const lowSec = Math.min(startSec, endSec);
      const highSec = Math.max(startSec, endSec);
      if (highSec < Number(visible.from) || lowSec > Number(visible.to)) return null;
    }
    return {
      priceRange: {
        minValue: this.spec.autoscalePrice,
        maxValue: this.spec.autoscalePrice,
      },
    };
  }
  setSpec(spec: RectSpec) {
    this.spec = spec;
    this.view?.refresh(spec);
    // refresh() already calls requestUpdate(), but cover the rare path
    // where setSpec runs before attached() (no-op safe).
    this.requestUpdate();
  }
}
