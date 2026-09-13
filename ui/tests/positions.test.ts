import assert from 'node:assert/strict';
import test from 'node:test';
import { historicalAnchorIndex,defaultPrices,movePrices,riskReward,validatePrices,type UserPosition } from '../src/components/chart/positions/model.ts';
import { PositionRepository, type PositionTransport } from '../src/components/chart/positions/repository.ts';
function row(id = 'p'): UserPosition { return { ...defaultPrices('long',100),id,symbol: 'EUR',source_alert_id: 'alert',drawn_tf: '5m',created_at_ts: 1,updated_at_ts: 1 }; }
function fixture(update?: PositionTransport['update']) {
  const errors: string[] = [], writes: UserPosition[] = [];
  let database = [row()]; let creates = 0;
  const api: PositionTransport = {
    list: async symbol => database.filter(p => p.symbol === symbol).map(p => ({ ...p })),
    create: async draft => { const id = `new-${++creates}`; database.push({ ...draft,id,created_at_ts: 1,updated_at_ts: 1 }); return id; },
    update: update ?? (async p => { writes.push(p); const ack = { ...p,updated_at_ts: p.updated_at_ts+1 }; database = database.map(old => old.id === p.id ? ack : old); return ack; }),
    delete: async id => { database = database.filter(p => p.id !== id); },
  };
  const repo = new PositionRepository(api,() => {},e => errors.push(e),10000);
  return { repo,api,errors,writes };
}
test('D4 long / short with and without target; invalid risk never yields NaN', () => {
  for (const side of ['long','short'] as const) {
    const p = defaultPrices(side,100);
    assert.equal(riskReward(p),'2.00'); assert.equal(validatePrices(p),null);
    assert.equal(riskReward({ ...p,target_price: null }),'—');
    assert.equal(riskReward({ ...p,stop_price: p.entry_price }),'—');
  }
  assert.ok(validatePrices({ ...row(),target_price: Infinity }));
});
test('D6 each line moves independently; translating all preserves signed distances and nullable target', () => {
  const p = row();
  for (const field of ['entry_price','stop_price','target_price'] as const) {
    const next = movePrices(p,field,0.01);
    for (const other of ['entry_price','stop_price','target_price'] as const) assert.equal(next[other],other === field ? p[other]!+0.01 : p[other]);
  }
  const translated = movePrices(p,'all',10);
  assert.equal(translated.stop_price-translated.entry_price,p.stop_price-p.entry_price);
  assert.equal(translated.target_price!-translated.entry_price,p.target_price!-p.entry_price);
  assert.equal(movePrices({ ...p,target_price: null },'all',10).target_price,null);
});
test('D6 debounce coalesces edits, skips unchanged replay, and shares one symbol cache across panes', async () => {
  const { repo,writes } = fixture(); await Promise.all([repo.load('EUR'),repo.load('EUR')]);
  repo.commit(movePrices(row(),'all',1)); repo.commit(movePrices(row(),'all',2));
  await repo.flushAll(); assert.equal(writes.length,1); assert.equal(writes[0].entry_price,102);
  repo.commit({ ...repo.rows.EUR[0] }); await repo.flushAll(); assert.equal(writes.length,1); assert.equal(repo.isSaving('p'),false);
  await repo.load('DXY'); assert.equal(repo.rows.DXY.length,0);
});
test('D6 stale acknowledgement cannot replace newer edit; writes are serialized', async () => {
  const resolvers: ((p: UserPosition) => void)[] = [];
  const requests: UserPosition[] = [];
  const { repo } = fixture(p => { requests.push(p); return new Promise(resolve => resolvers.push(resolve)); });
  await repo.load('EUR'); repo.commit(movePrices(row(),'all',1)); const first = repo.flush('p');
  repo.commit(movePrices(row(),'all',2)); assert.equal(requests.length,1);
  resolvers[0](requests[0]); await first;
  assert.equal(repo.rows.EUR[0].entry_price,102); assert.equal(requests.length,2);
  resolvers[1](requests[1]); await repo.flushAll(); assert.equal(repo.rows.EUR[0].entry_price,102);
});
test('D6 saved acknowledgement does not disturb active pointer preview', async () => {
  let resolve!: (p: UserPosition) => void;
  const { repo } = fixture(() => new Promise(r => { resolve=r; }));
  await repo.load('EUR'); const saved = movePrices(row(),'all',1); repo.commit(saved);
  const request = repo.flush('p'); repo.preview(movePrices(row(),'all',3)); resolve(saved); await request;
  assert.equal(repo.rows.EUR[0].entry_price,103);
  repo.restore(saved); await repo.flushAll(); assert.equal(repo.rows.EUR[0].entry_price,101);
});
test('D6 failed update rolls back and keeps source identity; failed delete restores', async () => {
  const { repo,api,errors } = fixture(async () => { throw new Error('disk full'); });
  await repo.load('EUR'); repo.commit(movePrices(row(),'all',1)); await repo.flushAll();
  assert.deepEqual(repo.rows.EUR[0],row()); assert.equal(errors.length,1);
  api.delete = async () => { throw new Error('locked'); };
  await assert.rejects(repo.delete(row())); assert.deepEqual(repo.rows.EUR[0],row());
});
test('D6 delete during update waits then deletes; acknowledgement cannot resurrect it', async () => {
  let resolve!: (p: UserPosition) => void;
  const { repo } = fixture(() => new Promise(r => { resolve=r; }));
  await repo.load('EUR'); const p = movePrices(row(),'all',1); repo.commit(p); const save = repo.flush('p');
  const deletion = repo.delete(p); assert.equal(repo.rows.EUR.length,0);
  resolve(p); await save; await deletion; await repo.flushAll(); assert.equal(repo.rows.EUR.length,0);
});
test('D7 new frontend repository restores all persisted rows and source_alert_id', async () => {
  const { repo,api } = fixture(); await repo.load('EUR');
  const p = await repo.create({ ...defaultPrices('short',100),symbol: 'EUR',source_alert_id: 'a2',drawn_tf: '1h' });
  repo.commit({ ...p,target_price: null }); await repo.flushAll();
  const reloaded = new PositionRepository(api,() => {},() => {}); await reloaded.load('EUR');
  assert.equal(reloaded.rows.EUR.length,2);
  assert.deepEqual(reloaded.rows.EUR,repo.rows.EUR);
  const restored = reloaded.rows.EUR.find(x => x.id === p.id)!;
  assert.equal(restored.source_alert_id,'a2'); assert.equal(restored.target_price,null); assert.equal(restored.drawn_tf,'1h');
});
test('failed create removes optimistic placeholder', async () => {
  const { repo,api } = fixture(); await repo.load('EUR');
  api.create = async () => { throw new Error('disk full'); };
  await assert.rejects(repo.create({ ...defaultPrices('long',100),symbol: 'EUR',source_alert_id: null,drawn_tf: '5m' }));
  assert.equal(repo.rows.EUR.length,1); assert.equal(repo.busy.size,0);
});

test('historical C2 close stays fixed when new candles arrive; TF changes use the same instant', () => {
 const m=60000, anchor=30*m;
 const five=Array.from({length:7},(_,i)=>i*5*m);
 assert.equal(historicalAnchorIndex(five,anchor,5*m),5);
 assert.equal(historicalAnchorIndex([...five,35*m,40*m],anchor,5*m),5);
 assert.equal(historicalAnchorIndex([0,15*m,30*m,45*m],anchor,15*m),1);
 assert.equal(historicalAnchorIndex([0,60*m],anchor,60*m),0);
 assert.equal(historicalAnchorIndex([0,5*m],anchor,5*m),null);
 assert.equal(historicalAnchorIndex([30*m,35*m],anchor,5*m),null);
 assert.equal(historicalAnchorIndex([0,5*m,60*m],anchor,5*m),null);
 assert.equal(movePrices({...row(),anchor_ts:anchor},'all',1).anchor_ts,anchor);
});
