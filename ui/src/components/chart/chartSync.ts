// Lightweight pub/sub for multi-pane chart sync (M5).
//
// Range sync is TIME-based (UTC epoch seconds), not logical-index, so panes
// with different bar counts (e.g. TVC:DXY 4h bars filtered from a 2h grid)
// stay aligned by wall-clock time rather than bar index.
//
// barSpacing is broadcast alongside the time range so the receiving pane can
// preserve the originator's zoom level. lightweight-charts' setVisibleRange /
// setVisibleLogicalRange ALWAYS recalculate bar spacing (= width / range
// length), which causes unwanted zoom. Instead, the receiver applies
// applyOptions({ barSpacing }) for zoom and scrollToPosition for scroll --
// neither of which touches the other.
//
// Range and crosshair use independent channels so a scroll does not re-apply
// a stale crosshair and a mouse-move does not re-broadcast a stale range.

export type TimeRange = { from: number; to: number; barSpacing: number };

const rangeListeners = new Set<(r: TimeRange | null) => void>();
const crosshairListeners = new Set<(t: number | null) => void>();

export const chartSync = {
  setRange: (r: TimeRange | null) => {
    rangeListeners.forEach((l) => l(r));
  },
  setCrosshair: (t: number | null) => {
    crosshairListeners.forEach((l) => l(t));
  },
  subscribeRange: (l: (r: TimeRange | null) => void) => {
    rangeListeners.add(l);
    return () => {
      rangeListeners.delete(l);
    };
  },
  subscribeCrosshair: (l: (t: number | null) => void) => {
    crosshairListeners.add(l);
    return () => {
      crosshairListeners.delete(l);
    };
  },
};
