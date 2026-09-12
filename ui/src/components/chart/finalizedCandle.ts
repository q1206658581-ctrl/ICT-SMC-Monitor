import type { CandlestickData, IChartApi, ISeriesApi, Time } from 'lightweight-charts';

/** Formal closes may arrive after the next quote candle is already visible. */
export function applyDelayedClose(
  series: ISeriesApi<'Candlestick'>,
  chart: IChartApi,
  candle: CandlestickData<Time>,
) {
  const data = series.data();
  let lo = 0;
  let hi = data.length;
  const time = Number(candle.time);
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (Number(data[mid].time) < time) lo = mid + 1;
    else hi = mid;
  }
  if (lo < data.length && Number(data[lo].time) === time) {
    series.update(candle, true);
    return;
  }
  // A quote-free minute has no point to update. Insert it into the sorted
  // series while preserving the user's visible candles and zoom.
  const visible = chart.timeScale().getVisibleLogicalRange();
  const merged = [...data];
  merged.splice(lo, 0, candle);
  series.setData(merged);
  if (visible) {
    chart.timeScale().setVisibleLogicalRange({
      from: visible.from + (lo <= visible.from ? 1 : 0),
      to: visible.to + (lo <= visible.to ? 1 : 0),
    });
  }
}
