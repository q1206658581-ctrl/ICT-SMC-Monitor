// Lightweight-charts v5 series primitive: a diagonal SMT sweep line
// with a zoom-scaling text label at the endpoint (§5.8).

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

function toSeconds(t: Time): number {
  if (typeof t === 'number') return t;
  if (typeof t === 'string') return Math.floor(new Date(t).getTime() / 1000);
  const bd = t as { year: number; month: number; day: number };
  return Math.floor(new Date(bd.year, bd.month - 1, bd.day).getTime() / 1000);
}

export type SmtSweepSpec = {
  startTime: Time;
  endTime: Time;
  startPrice: number;
  endPrice: number;
  label: string;
  color: string;
};

type VisibleEdges = {
  left: number;
  right: number;
  leftSec: number;
  rightSec: number;
  barSpacing: number;
} | null;

class SmtSweepRenderer implements IPrimitivePaneRenderer {
  constructor(
    spec: SmtSweepSpec,
    toX: (t: Time) => number | null,
    toY: (p: number) => number | null,
    visibleEdges: () => VisibleEdges,
  ) {
    this.spec = spec;
    this.toX = toX;
    this.toY = toY;
    this.visibleEdges = visibleEdges;
  }

  private spec: SmtSweepSpec;
  private toX: (t: Time) => number | null;
  private toY: (p: number) => number | null;
  private visibleEdges: () => VisibleEdges;

  draw(target: PrimitiveDrawTarget) {
    target.useBitmapCoordinateSpace((scope: {
      context: CanvasRenderingContext2D;
      horizontalPixelRatio: number;
      verticalPixelRatio: number;
    }) => {
      const { spec } = this;
      const ctx = scope.context;
      const hpr = scope.horizontalPixelRatio;
      const vpr = scope.verticalPixelRatio;

      const edges = this.visibleEdges();
      if (!edges) return;

      const startSec = toSeconds(spec.startTime);
      const endSec = toSeconds(spec.endTime);

      let x1 = this.toX(spec.startTime);
      let x2 = this.toX(spec.endTime);
      const y1 = this.toY(spec.startPrice);
      const y2 = this.toY(spec.endPrice);
      if (y1 === null || y2 === null) return;

      // Clamp off-screen X coordinates to visible edges
      const clampX = (sec: number, x: number | null): number => {
        if (x !== null) return x;
        const span = Math.max(1, edges.rightSec - edges.leftSec);
        const ratio = (sec - edges.leftSec) / span;
        return Math.max(edges.left, Math.min(edges.right, edges.left + (edges.right - edges.left) * ratio));
      };
      // Skip entirely if both points are on the same side outside the view
      if (startSec < edges.leftSec && endSec < edges.leftSec) return;
      if (startSec > edges.rightSec && endSec > edges.rightSec) return;

      x1 = clampX(startSec, x1);
      x2 = clampX(endSec, x2);

      // --- Draw the diagonal white line ---
      ctx.save();
      ctx.strokeStyle = spec.color;
      ctx.lineWidth = 2 * hpr;
      ctx.lineCap = 'round';
      ctx.beginPath();
      ctx.moveTo(x1 * hpr, y1 * vpr);
      ctx.lineTo(x2 * hpr, y2 * vpr);
      ctx.stroke();

      // --- Draw the label at the endpoint (horizontal, above the line) ---
      // Font size scales with barSpacing so it zooms in/out with the chart
      // and never covers other elements at high zoom.
      const fontSize = Math.max(9, Math.min(20, edges.barSpacing * 0.55)) * vpr;
      ctx.fillStyle = spec.color;
      ctx.font = `${fontSize}px sans-serif`;
      ctx.textBaseline = 'bottom';
      ctx.textAlign = 'left';
      const labelX = (x2 + 4) * hpr;
      const labelY = (y2 - 4) * vpr;
      ctx.fillText(spec.label, labelX, labelY);
      ctx.restore();
    });
  }
}

class SmtSweepPaneView implements IPrimitivePaneView {
  private chart: IChartApi;
  private series: ISeriesApi<SeriesType, Time>;
  spec: SmtSweepSpec;
  private rendererInstance: SmtSweepRenderer;
  private requestUpdate: () => void;
  private rangeUnsub: (() => void) | null = null;

  constructor(
    chart: IChartApi,
    series: ISeriesApi<SeriesType, Time>,
    spec: SmtSweepSpec,
    requestUpdate: () => void,
  ) {
    this.chart = chart;
    this.series = series;
    this.spec = spec;
    this.requestUpdate = requestUpdate;
    this.rendererInstance = this.makeRenderer();
    const ts = chart.timeScale();
    const onRange = () => {
      this.rendererInstance = this.makeRenderer();
      this.requestUpdate();
    };
    ts.subscribeVisibleTimeRangeChange(onRange);
    ts.subscribeVisibleLogicalRangeChange(onRange);
    this.rangeUnsub = () => {
      ts.unsubscribeVisibleTimeRangeChange(onRange);
      ts.unsubscribeVisibleLogicalRangeChange(onRange);
    };
  }

  dispose() {
    if (this.rangeUnsub) { this.rangeUnsub(); this.rangeUnsub = null; }
  }

  zOrder(): PrimitivePaneViewZOrder {
    return 'top';
  }

  private makeRenderer() {
    const ts = this.chart.timeScale();
    return new SmtSweepRenderer(
      this.spec,
      (t) => ts.timeToCoordinate(t),
      (p) => this.series.priceToCoordinate(p),
      () => {
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
          barSpacing: Math.abs(right - left) / Math.max(1, Math.abs(Number(range.to) - Number(range.from))),
        };
      },
    );
  }

  refresh(spec: SmtSweepSpec, requestUpdate = true) {
    this.spec = spec;
    this.rendererInstance = this.makeRenderer();
    if (requestUpdate) this.requestUpdate();
  }

  renderer(): IPrimitivePaneRenderer {
    return this.rendererInstance;
  }
}

export class SmtSweepPrimitive implements ISeriesPrimitive<Time> {
  spec: SmtSweepSpec;
  private view: SmtSweepPaneView | null = null;
  private chart: IChartApi | null = null;
  private requestUpdate: () => void = () => {};

  constructor(spec: SmtSweepSpec) {
    this.spec = spec;
  }

  attached(param: SeriesAttachedParameter<Time, SeriesType>) {
    this.chart = param.chart;
    this.requestUpdate = param.requestUpdate;
    this.view = new SmtSweepPaneView(param.chart, param.series, this.spec, param.requestUpdate);
  }

  detached() {
    this.view?.dispose();
    this.view = null;
    this.chart = null;
    this.requestUpdate = () => {};
  }

  updateAllViews() {
    this.view?.refresh(this.spec, false);
  }

  paneViews(): readonly IPrimitivePaneView[] {
    return this.view ? [this.view] : [];
  }

  autoscaleInfo() {
    // Only a white sweep line intersecting the current time window may widen
    // the Y axis. Keeping every historical SMT line in autoscale compressed a
    // focused setup even though those old lines were completely off-screen.
    const visible = this.chart?.timeScale().getVisibleRange();
    if (visible) {
      const startSec = toSeconds(this.spec.startTime);
      const endSec = toSeconds(this.spec.endTime);
      const lowSec = Math.min(startSec, endSec);
      const highSec = Math.max(startSec, endSec);
      if (highSec < Number(visible.from) || lowSec > Number(visible.to)) return null;
    }
    const lo = Math.min(this.spec.startPrice, this.spec.endPrice);
    const hi = Math.max(this.spec.startPrice, this.spec.endPrice);
    return { priceRange: { minValue: lo, maxValue: hi } };
  }

  setSpec(spec: SmtSweepSpec) {
    this.spec = spec;
    this.view?.refresh(spec);
    this.requestUpdate();
  }
}
