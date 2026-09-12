import assert from 'node:assert/strict';
import test from 'node:test';
import { resolveAlertSmt } from '../src/components/chart/alertNavigation.ts';

test('filtered invalidated SMT remains navigable by its original ID', async () => {
  const snapshot = { id: 'original', htf_confirmed: false, detection_state: 'invalidated',
    chains: [{ symbol: 'AUDUSD', c2_candle: { ts: 1789023600000 } }] };
  const result = await resolveAlertSmt('original', [{ id: 'replacement' }], async (id) => {
    assert.equal(id, 'original');
    return snapshot;
  });
  assert.equal(result, snapshot);
});

test('visible snapshot needs no fallback and missing snapshot stays missing', async () => {
  const snapshot = { id: 'visible' };
  assert.equal(await resolveAlertSmt('visible', [snapshot], async () => {
    throw new Error('should not load');
  }), snapshot);
  assert.equal(await resolveAlertSmt('missing', [], async () => null), null);
});
