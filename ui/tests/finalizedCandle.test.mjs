import assert from 'node:assert/strict';
import test from 'node:test';
import { applyDelayedClose } from '../src/components/chart/finalizedCandle.ts';

const candle = (time, close = 1) => ({ time, open: 1, high: 2, low: 0, close });

function fixture(times, range) {
  let data = times.map((time) => candle(time));
  let visible = range;
  let historicalUpdates = 0;
  const series = {
    data: () => data,
    update: (bar, historical) => {
      assert.equal(historical, true);
      historicalUpdates += 1;
      data = data.map((old) => old.time === bar.time ? bar : old);
    },
    setData: (bars) => { data = bars; },
  };
  const scale = {
    getVisibleLogicalRange: () => visible,
    setVisibleLogicalRange: (value) => { visible = value; },
  };
  return { series, chart: { timeScale: () => scale },
    data: () => data, visible: () => visible, updates: () => historicalUpdates };
}

test('late formal close corrects an older candle without replacing the live edge', () => {
  const f = fixture([60, 120, 180], { from: 0, to: 2 });
  applyDelayedClose(f.series, f.chart, candle(120, 1.5));
  assert.equal(f.updates(), 1);
  assert.equal(f.data()[1].close, 1.5);
  assert.deepEqual(f.data()[2], candle(180));
  assert.deepEqual(f.visible(), { from: 0, to: 2 });
});

test('a minute missed by quotes is inserted once in timestamp order', () => {
  const f = fixture([60, 180, 240], { from: 1, to: 2 });
  applyDelayedClose(f.series, f.chart, candle(120));
  assert.deepEqual(f.data().map((b) => b.time), [60, 120, 180, 240]);
  assert.deepEqual(f.visible(), { from: 2, to: 3 });
  applyDelayedClose(f.series, f.chart, candle(120, 1.6));
  assert.equal(f.data().length, 4);
  assert.equal(f.data()[1].close, 1.6);
});

test('insertion within the viewport keeps its existing boundary candles visible', () => {
  const f = fixture([60, 180, 240], { from: 0, to: 2 });
  applyDelayedClose(f.series, f.chart, candle(120));
  assert.deepEqual(f.visible(), { from: 0, to: 3 });
});
