import { samePrices, validatePrices, type PositionDraft, type UserPosition } from './model.ts';
export interface PositionTransport {
  list(symbol: string): Promise<UserPosition[]>;
  create(draft: PositionDraft): Promise<string>;
  update(position: UserPosition): Promise<UserPosition>;
  delete(id: string): Promise<void>;
}
/** One cache per app, shared across TFs/panes. Serialized writes prevent stale acknowledgements. */
export class PositionRepository {
  rows: Record<string, UserPosition[]> = {};
  readonly busy = new Set<string>();
  private loaded = new Set<string>();
  private loading = new Map<string, Promise<void>>();
  private confirmed = new Map<string, UserPosition>();
  private desired = new Map<string, UserPosition>();
  private timers = new Map<string, ReturnType<typeof setTimeout>>();
  private running = new Map<string, Promise<void>>();
  private deleting = new Set<string>();
  private editing = new Set<string>();
  private api: PositionTransport;
  private changed: () => void;
  private failed: (message: string) => void;
  private delay: number;
  constructor(api: PositionTransport, changed: () => void, failed: (message: string) => void, delay = 250) {
    this.api = api; this.changed = changed; this.failed = failed; this.delay = delay;
  }
  private put(p: UserPosition) {
    const rows = this.rows[p.symbol] ?? [];
    this.rows = { ...this.rows, [p.symbol]: rows.some(r => r.id === p.id) ? rows.map(r => r.id === p.id ? p : r) : [...rows,p] };
    this.changed();
  }
  private remove(p: UserPosition) {
    this.rows = { ...this.rows, [p.symbol]: (this.rows[p.symbol] ?? []).filter(r => r.id !== p.id) };
    this.changed();
  }
  load(symbol: string): Promise<void> {
    if (this.loaded.has(symbol)) return Promise.resolve();
    const active = this.loading.get(symbol);
    if (active) return active;
    const request = this.api.list(symbol).then(rows => {
      this.rows = { ...this.rows, [symbol]: rows };
      rows.forEach(p => this.confirmed.set(p.id,p));
      this.loaded.add(symbol); this.changed();
    }).finally(() => this.loading.delete(symbol));
    this.loading.set(symbol,request);
    return request;
  }
  async create(draft: PositionDraft): Promise<UserPosition> {
    const error = validatePrices(draft); if (error) throw new Error(error);
    await this.load(draft.symbol);
    const now = Date.now();
    const temp: UserPosition = { ...draft, id: `pending-${crypto.randomUUID()}`, created_at_ts: now, updated_at_ts: now };
    this.busy.add(temp.id); this.put(temp);
    try {
      const id = await this.api.create(draft);
      let p = { ...temp,id };
      try {
        // The create command returns an ID; read back server timestamps without
        // replacing other rows that may have changed during this request.
        p = (await this.api.list(draft.symbol)).find(row => row.id === id) ?? p;
      } catch (error) { this.failed(`仓位已保存，但确认信息读取失败；重载后可恢复：${String(error)}`); }
      this.remove(temp); this.confirmed.set(id,p); this.put(p);
      return p;
    } catch (e) { this.remove(temp); throw e; }
    finally { this.busy.delete(temp.id); this.changed(); }
  }
  preview(p: UserPosition) { if (!this.deleting.has(p.id) && !this.busy.has(p.id)) { this.editing.add(p.id); this.put(p); } }
  restore(p: UserPosition) { this.editing.delete(p.id); this.put(p); }
  commit(p: UserPosition) {
    if (this.deleting.has(p.id) || this.busy.has(p.id)) return;
    const error = validatePrices(p); if (error) throw new Error(error);
    this.editing.delete(p.id); this.put(p); this.desired.set(p.id,p);
    clearTimeout(this.timers.get(p.id));
    this.timers.set(p.id,setTimeout(() => { this.timers.delete(p.id); void this.flush(p.id); },this.delay));
  }
  flush(id: string): Promise<void> {
    const active = this.running.get(id); if (active) return active;
    const next = this.desired.get(id);
    if (!next || this.deleting.has(id)) return Promise.resolve();
    this.desired.delete(id);
    if (this.confirmed.has(id) && samePrices(this.confirmed.get(id)!,next)) { this.changed(); return Promise.resolve(); }
    const request = this.api.update(next).then(ack => {
      this.confirmed.set(id,ack);
      if (!this.desired.has(id) && !this.deleting.has(id) && !this.editing.has(id)) this.put(ack);
    }).catch(error => {
      if (!this.desired.has(id) && !this.deleting.has(id) && !this.editing.has(id)) {
        const old = this.confirmed.get(id); if (old) this.put(old);
      }
      this.failed(`仓位保存失败，未保存的改动已回退或等待后续保存：${String(error)}`);
    }).finally(() => {
      this.running.delete(id); this.changed();
      if (this.desired.has(id)) void this.flush(id);
    });
    this.running.set(id,request);
    return request;
  }
  async delete(p: UserPosition) {
    if (this.busy.has(p.id) || this.deleting.has(p.id)) return;
    this.deleting.add(p.id); this.desired.delete(p.id);
    clearTimeout(this.timers.get(p.id)); this.timers.delete(p.id); this.remove(p);
    await this.running.get(p.id);
    try { await this.api.delete(p.id); this.confirmed.delete(p.id); }
    catch (error) { this.put(this.confirmed.get(p.id) ?? p); throw error; }
    finally { this.deleting.delete(p.id); }
  }
  isSaving(id: string) { return this.busy.has(id) || this.desired.has(id) || this.running.has(id); }
  async flushAll() {
    this.timers.forEach(clearTimeout); this.timers.clear();
    while (this.desired.size || this.running.size) await Promise.all([...this.desired.keys()].map(id => this.flush(id)).concat([...this.running.values()]));
  }
}
