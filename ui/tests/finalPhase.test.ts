import assert from 'node:assert/strict';
import test from 'node:test';
import { buildCorrelationPairs, confirmedCorrelations } from '../src/components/layout/correlationDraft.ts';

// Load the real store with isolated in-memory persistence, never the app's localStorage.
const values = new Map<string, string>();
Object.defineProperty(globalThis, 'localStorage', { value: {
  getItem: (key: string) => values.get(key) ?? null,
  setItem: (key: string, value: string) => { values.set(key, value); },
  removeItem: (key: string) => { values.delete(key); },
}, configurable: true });
Object.defineProperty(globalThis, 'window', { value: { localStorage: globalThis.localStorage }, configurable: true });
const { useChartStore } = await import('../src/store/index.ts');

test('outside-group symbol survives layout round trip; group change resets selection', () => {
  const store = useChartStore;
  store.getState().setActiveWatchlist({ id: 'eu', name: 'EU', symbols: ['OANDA:EURUSD', 'TVC:DXY'], correlations: [] });
  store.setState({ symbol: 'OANDA:XAUUSD', singleMode: true });
  store.getState().setSingleMode(false);
  assert.equal(store.getState().symbol, 'OANDA:XAUUSD');
  assert.equal(store.getState().singleMode, false);
  store.getState().setSingleMode(true);
  assert.equal(store.getState().symbol, 'OANDA:XAUUSD');
  store.getState().setActiveWatchlist({ id: 'aud', name: 'AUD', symbols: ['OANDA:AUDUSD', 'OANDA:NZDUSD'], correlations: [] });
  store.getState().setSingleMode(true);
  assert.equal(store.getState().symbol, 'OANDA:AUDUSD');
});

test('new correlations cannot be saved before explicit selection', () => {
  const pairs = buildCorrelationPairs(['OANDA:XAUUSD', 'COINBASE:BTCUSD'], []);
  assert.equal(pairs[0].direction, '');
  assert.throws(() => confirmedCorrelations(pairs), /请选择/);
  pairs[0].direction = 'negative';
  assert.equal(confirmedCorrelations(pairs)[0].direction, 'negative');
  assert.deepEqual(confirmedCorrelations(buildCorrelationPairs(['OANDA:XAUUSD'], pairs)), []);
});

test('adding a third symbol retains user choices but leaves new pairs unresolved', () => {
  const existing = [{ a: 'OANDA:EURUSD', b: 'TVC:DXY', direction: 'negative' as const }];
  const pairs = buildCorrelationPairs(['TVC:DXY', 'OANDA:EURUSD', 'OANDA:XAUUSD'], existing);
  assert.equal(pairs.find((p) => p.a === 'OANDA:EURUSD' && p.b === 'TVC:DXY')?.direction, 'negative');
  assert.equal(pairs.filter((p) => p.direction === '').length, 2);
  assert.throws(() => confirmedCorrelations(pairs), /请选择/);
});
