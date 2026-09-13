import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from 'react';
import type { IChartApi, ISeriesApi, Logical } from 'lightweight-charts';
import { toast } from 'sonner';
import { defaultPrices, historicalAnchorIndex, movePrices, riskReward, validatePrices, type PriceField, type UserPosition } from './model';
import { positions, usePositions } from './store';

export interface PositionChart { chart: IChartApi; series: ISeriesApi<'Candlestick'> }
interface Geometry { width: number; height: number; lefts: Record<string, number | null>; levels: Record<string, (number | null)[]> }
const EMPTY: UserPosition[] = [];
type DragField = PriceField | 'all' | 'new_target';
const durations: Record<string,number> = { '1m': 60000,'5m': 300000,'15m': 900000,'30m': 1800000,'1h': 3600000,'4h': 14400000,'1d': 86400000,'1w': 604800000 };
const labels: Record<PriceField,string> = { entry_price: '入场', stop_price: '止损', target_price: '止盈' };
export function PositionLayer({ api, symbol, tf, paneIndex, ready }: {
  api: PositionChart; symbol: string; tf: string; paneIndex: number; ready: boolean;
}) {
  const rows = usePositions(s => s.rows[symbol] ?? EMPTY);
  const mode = usePositions(s => s.mode);
  usePositions(s => s.revision);
  const selected = usePositions(s => s.selected);
  const owner = usePositions(s => s.owner);
  const [loadError,setLoadError] = useState('');
  const [preview,setPreview] = useState<UserPosition | null>(null);
  const [geometry,setGeometry] = useState<Geometry>({ width: 0,height: 0,lefts: {},levels: {} });
  const [editor,setEditor] = useState<{ id: string; field: PriceField; value: string; error: string } | null>(null);
  const [menu,setMenu] = useState<{ id: string; x: number; y: number } | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const rowRef = useRef(rows); rowRef.current = rows;
  const previewRef = useRef(preview); previewRef.current = preview;
  const dragging = useRef<{ original: UserPosition; field: DragField; start: number; startY: number; current: UserPosition; pointer: number; element: Element } | null>(null);
  const current = rows.find(p => p.id === selected);
  const hasManualPositions = rows.some(p => !p.source_alert_id);
  const format = (value: number) => api.series.priceFormatter().format(value);
  const revealRightSpace = () => {
    const ts = api.chart.timeScale();
    const bar = api.series.data().at(-1);
    if (!bar) return;
    const x = ts.timeToCoordinate(bar.time);
    const spacing = ts.options().barSpacing;
    const space = Math.min(240,ts.width()*0.45);
    if (x === null || x < 0 || ts.width()-x < space+spacing/2+6) {
      ts.scrollToPosition((space+6)/spacing+1,false);
    }
  };
  const focus = (p: UserPosition) => {
    usePositions.getState().select(p.id,paneIndex);
    if (p.source_alert_id) {
      const index = p.anchor_ts == null ? null : historicalAnchorIndex(api.series.data().map(bar => Number(bar.time)*1000),p.anchor_ts,durations[tf]);
      if (index === null) {
        toast.info(p.anchor_ts == null ? '该历史仓位缺少 C2 确认时间，无法定位' : '当前已加载的 K 线未覆盖该告警时刻，请切换到覆盖该时刻的周期');
        return;
      }
      api.chart.timeScale().setVisibleLogicalRange({ from: index-35,to: index+55 });
    } else revealRightSpace();
    const values = [p.entry_price,p.stop_price,...(p.target_price === null ? [] : [p.target_price])];
    const min = Math.min(...values),max = Math.max(...values),pad = Math.max((max-min)*0.3,Math.abs(min)*0.0001,0.00001);
    api.chart.priceScale('right').setVisibleRange({ from: min-pad,to: max+pad });
  };

  useEffect(() => {
    let active = true;
    void positions.load(symbol).then(() => { if (active) setLoadError(''); }).catch(error => {
      if (active) setLoadError(`仓位加载失败：${String(error)}`);
    });
    return () => { active = false; };
  },[symbol]);

  useEffect(() => {
    // Reserve room once on load/placement, without fighting subsequent manual panning.
    if (!ready || (!hasManualPositions && !mode)) return;
    const frame = requestAnimationFrame(revealRightSpace);
    return () => cancelAnimationFrame(frame);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  },[ready,hasManualPositions,mode,api]);

  useEffect(() => {
    let frame = 0, last = '';
    const paint = () => {
      const width = api.chart.timeScale().width();
      const height = api.chart.panes()[0]?.getHeight() ?? 0;
      const levels: Geometry['levels'] = {};
      const lefts: Geometry['lefts'] = {};
      const ts = api.chart.timeScale();
      const data = api.series.data();
      const times = data.map(bar => Number(bar.time)*1000);
      for (const p of [...rowRef.current,...(previewRef.current ? [previewRef.current] : [])]) {
        const index = p.source_alert_id
          ? (p.anchor_ts == null ? null : historicalAnchorIndex(times,p.anchor_ts,durations[tf]))
          : data.length-1;
        const x = index === null || index < 0 ? null : ts.logicalToCoordinate(index as Logical);
        lefts[p.id] = x === null ? null : Math.max(0,x+ts.options().barSpacing/2+6);
        levels[p.id] = [p.entry_price,p.stop_price,p.target_price].map(price => price === null ? null : api.series.priceToCoordinate(price));
      }
      const next = { width,height,lefts,levels };
      const key = JSON.stringify(next);
      if (key !== last) { last = key; setGeometry(next); }
      frame = requestAnimationFrame(paint);
    };
    frame = requestAnimationFrame(paint);
    return () => cancelAnimationFrame(frame);
  },[api,tf]);

  useEffect(() => {
    // Claim keyboard ownership only in a pane that actually contains this symbol.
    const p = rowRef.current.find(p => p.id === selected);
    if (p && owner === null) usePositions.getState().select(p.id,paneIndex);
    if (p && ready && !dragging.current) {
      // Parent loads setData in an effect: navigate on the next frame so it cannot overwrite the jump.
      const frame = requestAnimationFrame(() => focus(p));
      return () => cancelAnimationFrame(frame);
    }
    // Fit on selection/target change, never continuously while dragging.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  },[selected,current?.id,symbol,tf,ready]);

  const cancelDrag = () => {
    const drag = dragging.current; dragging.current = null;
    if (drag) {
      positions.restore(drag.original);
      if (drag.element.hasPointerCapture(drag.pointer)) drag.element.releasePointerCapture(drag.pointer);
    }
  };
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.target as HTMLElement)?.closest('input,textarea,select,[contenteditable="true"],[role="dialog"]')) return;
      if (event.key === 'Escape') {
        cancelDrag(); setPreview(null); setEditor(null); setMenu(null);
        usePositions.getState().setMode(null);
      }
      const state = usePositions.getState();
      if (event.key === 'Delete' && state.owner === paneIndex && !dragging.current) {
        const p = rowRef.current.find(p => p.id === state.selected);
        if (p) { event.preventDefault(); void remove(p); }
      }
    };
    document.addEventListener('keydown',onKey);
    return () => {
      document.removeEventListener('keydown',onKey); cancelDrag();
      // Flush pending edits when changing TF / closing a chart pane.
      void positions.flushAll();
    };
  },[symbol,tf,paneIndex]);

  async function remove(p: UserPosition) {
    setMenu(null); setEditor(null);
    try { await positions.delete(p); if (usePositions.getState().selected === p.id) usePositions.getState().select(null,null); }
    catch (error) { toast.error(`删除失败，仓位已恢复：${String(error)}`); }
  }
  function priceAt(clientY: number) {
    const bounds = root.current?.getBoundingClientRect();
    return bounds ? api.series.coordinateToPrice(clientY-bounds.top) : null;
  }
  function begin(event: ReactPointerEvent, p: UserPosition, field: DragField) {
    if (event.button !== 0 || mode || positions.busy.has(p.id)) return;
    const start = priceAt(event.clientY); if (start === null) return;
    event.preventDefault(); event.stopPropagation();
    setMenu(null); usePositions.getState().select(p.id,paneIndex);
    api.chart.priceScale('right').applyOptions({ autoScale: false });
    event.currentTarget.setPointerCapture(event.pointerId);
    dragging.current = { original: p,current: p,field,start,startY: event.clientY,pointer: event.pointerId,element: event.currentTarget };
    positions.preview(p);
  }
  function move(event: ReactPointerEvent) {
    const drag = dragging.current; if (!drag || event.pointerId !== drag.pointer) return;
    const price = priceAt(event.clientY); if (price === null) return;
    event.preventDefault(); event.stopPropagation();
    // A click/double click must not manufacture a target before the user actually drags.
    if (drag.field === 'new_target' && Math.abs(event.clientY-drag.startY) < 3) return;
    const candidate = drag.field === 'new_target'
      ? { ...drag.original,target_price: price }
      : movePrices(drag.original,drag.field,price-drag.start);
    if (!validatePrices(candidate)) { drag.current = candidate; positions.preview(candidate); }
    else if (drag.field === 'new_target') { drag.current = drag.original; positions.preview(drag.original); }
  }
  function finish(event: ReactPointerEvent) {
    const drag = dragging.current; if (!drag || event.pointerId !== drag.pointer) return;
    event.stopPropagation(); dragging.current = null;
    if (drag.element.hasPointerCapture(drag.pointer)) drag.element.releasePointerCapture(drag.pointer);
    positions.commit(drag.current);
  }
  function edit(p: UserPosition,field: PriceField) {
    usePositions.getState().select(p.id,paneIndex);
    setEditor({ id: p.id,field,value: p[field] === null ? '' : String(p[field]),error: '' }); setMenu(null);
  }
  const interaction = (p: UserPosition,field: DragField) => ({
    onPointerDown: (event: ReactPointerEvent) => begin(event,p,field), onPointerMove: move, onPointerUp: finish,
    onPointerCancel: cancelDrag, onLostPointerCapture: () => { if (dragging.current) cancelDrag(); },
    onClick: (event: React.MouseEvent) => event.stopPropagation(),
    onContextMenu: (event: React.MouseEvent) => {
      event.preventDefault(); event.stopPropagation(); if (positions.busy.has(p.id)) return;
      usePositions.getState().select(p.id,paneIndex);
      const b = root.current!.getBoundingClientRect();
      setMenu({ id: p.id,x: Math.max(4,Math.min(event.clientX-b.left,geometry.width-155)),y: Math.max(4,Math.min(event.clientY-b.top,geometry.height-95)) });
    },
    onDoubleClick: (event: React.MouseEvent) => { event.stopPropagation(); if (!positions.busy.has(p.id)) edit(p,field === 'all' ? 'entry_price' : field === 'new_target' ? 'target_price' : field); },
  });
  const visible = [...rows].sort((a,b) => Number(a.id === selected)-Number(b.id === selected));
  if (preview && mode) visible.push(preview);
  return <div ref={root} data-position-layer className="absolute inset-0" style={{ pointerEvents: 'none',zIndex: 12 }}>
    <div className="absolute inset-0 overflow-hidden" style={{ width: geometry.width,height: geometry.height }}>
      {ready && visible.map(p => {
        const ys = geometry.levels[p.id]; if (!ys || ys[0] === null || ys[1] === null) return null;
        const [entry,stop,target] = ys as [number,number,number | null];
        const ghost = p.id === 'preview';
        const active = selected === p.id || ghost;
        const left = geometry.lefts[p.id];
        if (left == null || left >= geometry.width-18) return null;
        const width = geometry.width-left-18;
        return <div key={p.id} data-position-id={p.id} style={{ opacity: ghost || positions.busy.has(p.id) ? 0.6 : 1 }}>
          <div style={{ position: 'absolute',left,width,top: Math.min(entry,stop),height: Math.abs(entry-stop),background: active ? 'rgba(239,83,80,.26)' : 'rgba(239,83,80,.12)',borderLeft: `2px solid ${active ? '#4f8cff' : '#ef5350'}` }} />
          {target !== null && <div style={{ position: 'absolute',left,width,top: Math.min(entry,target),height: Math.abs(entry-target),background: active ? 'rgba(38,166,154,.26)' : 'rgba(38,166,154,.12)',borderLeft: `2px solid ${active ? '#4f8cff' : '#26a69a'}` }} />}
          {(['entry_price','stop_price','target_price'] as PriceField[]).map((field,i) => {
            const y = ys[i]; const price = p[field]; if (y === null || price === null) return null;
            const color = i === 0 ? '#aeb6c3' : i === 1 ? '#ef5350' : '#26a69a';
            return <div key={field} {...(ghost ? {} : interaction(p,field))} title={`${labels[field]} ${format(price)}；拖动调整，双击精确改价`}
              style={{ position: 'absolute',left,width,top: y-7,height: 14,pointerEvents: ghost || mode ? 'none' : 'auto',cursor: 'ns-resize',touchAction: 'none',userSelect: 'none' }}>
              <div style={{ position: 'absolute',top: 7,width: '100%',borderTop: `${active ? 2 : 1}px solid ${color}` }} />
              <span style={{ position: 'absolute',right: 0,maxWidth: '100%',overflow: 'hidden',whiteSpace: 'nowrap',top: i === 2 && target! > entry ? 8 : -12,fontSize: 10,color,background: '#101419',padding: '0 3px' }}>{labels[field]} {format(price)}</span>
            </div>;
          })}
          <div {...(ghost ? {} : interaction(p,'all'))} title="拖动此标签可整体平移；右键管理仓位；双击修改入场价"
            style={{ position: 'absolute',left,maxWidth: width,overflow: 'hidden',whiteSpace: 'nowrap',top: entry+10,fontSize: 11,lineHeight: '20px',padding: '0 5px',border: `1px solid ${active ? '#4f8cff' : '#48515e'}`,borderRadius: 3,color: '#eee',background: '#151a21',pointerEvents: ghost || mode ? 'none' : 'auto',cursor: 'move',touchAction: 'none',userSelect: 'none' }}>
            {p.source_alert_id ? '历史 · ' : ''}{p.side === 'long' ? '多头' : '空头'} · RR {riskReward(p)}{p.target_price === null ? ' · 未设止盈' : ''}{positions.isSaving(p.id) ? ' · 保存中' : ''}
          </div>
          {!ghost && (p.target_price === null || (dragging.current?.field === 'new_target' && dragging.current.original.id === p.id)) &&
            <div key="target-handle" {...interaction(p,'new_target')} data-add-target
              title={`按住拖动设置止盈：${p.side === 'long' ? '向上' : '向下'}拖至目标价格，松开保存；Esc 取消；双击可输入价格`}
              style={{ position: 'absolute',left,maxWidth: width,top: entry+(p.side === 'long' ? -38 : 38),overflow: 'hidden',whiteSpace: 'nowrap',fontSize: 11,lineHeight: '22px',padding: '0 6px',border: '1px dashed #26a69a',borderRadius: 3,color: '#54cebc',background: '#101b1c',pointerEvents: mode ? 'none' : 'auto',cursor: 'ns-resize',touchAction: 'none',userSelect: 'none' }}>
              ＋ {p.side === 'long' ? '↑' : '↓'} 拖动设止盈
            </div>}
        </div>;
      })}
    </div>
    {mode && ready && <div className="absolute left-0 top-0" style={{ width: geometry.width,height: geometry.height,pointerEvents: 'auto',cursor: 'crosshair',touchAction: 'none' }}
      onPointerEnter={() => {
        const bar = api.series.data().at(-1);
        if (bar && 'close' in bar) setPreview({ ...defaultPrices(mode,bar.close),id: 'preview',symbol,source_alert_id: null,drawn_tf: tf,created_at_ts: 0,updated_at_ts: 0 });
      }}
      onPointerMove={event => {
        const entry = priceAt(event.clientY); if (entry === null) return;
        setPreview({ ...defaultPrices(mode,entry),id: 'preview',symbol,source_alert_id: null,drawn_tf: tf,created_at_ts: 0,updated_at_ts: 0 });
      }}
      onPointerLeave={() => setPreview(null)}
      onPointerDown={event => {
        if (event.button !== 0) return;
        event.preventDefault(); event.stopPropagation();
        const entry = priceAt(event.clientY); if (entry === null) return;
        const draft = { ...defaultPrices(mode,entry),symbol,source_alert_id: null,drawn_tf: tf };
        usePositions.getState().setMode(null); setPreview(null);
        void positions.create(draft).then(p => usePositions.getState().select(p.id,paneIndex)).catch(error => toast.error(`创建仓位失败：${String(error)}`));
      }} />}
    <div className="absolute left-2 top-2 flex max-w-[95%] flex-wrap gap-1 text-[11px] text-text-1" style={{ pointerEvents: 'auto' }}>
      {mode ? <span className="rounded bg-bg-2 px-2 py-1">{mode === 'long' ? '多头' : '空头'}：点击图表放置 · Esc 取消{!ready ? ' · 等待行情加载' : ''}</span> : rows.length > 0 && <>
        <select aria-label="选择仓位" className="max-w-[150px] rounded border border-border bg-bg-1 px-1" value={current?.id ?? ''}
          onChange={event => { const p = rows.find(p => p.id === event.target.value); if (p) focus(p); else usePositions.getState().select(null,null); }}>
          <option value="">仓位（{rows.length}）</option>
          {rows.map((p,i) => <option key={p.id} value={p.id}>{i+1}. {p.side === 'long' ? '多头' : '空头'} {format(p.entry_price)}</option>)}
        </select>
        {current && !positions.busy.has(current.id) && <>
          <button type="button" className="rounded bg-bg-2 px-1" onClick={() => focus(current)}>定位</button>
          <button type="button" className="rounded bg-bg-2 px-1" onClick={() => edit(current,'target_price')}>{current.target_price === null ? '输入止盈' : '改止盈'}</button>
          <button type="button" className="rounded bg-bg-2 px-1 text-red-400" onClick={() => void remove(current)}>删除</button>
        </>}
      </>}
      {loadError && <button className="rounded bg-bg-1 text-red-400" onClick={() => void positions.load(symbol).then(() => setLoadError('')).catch(e => setLoadError(String(e)))}>{loadError} · 重试</button>}
    </div>
    {menu && <div className="absolute rounded border border-border bg-bg-1 p-1 text-xs shadow-lg" style={{ left: menu.x,top: menu.y,pointerEvents: 'auto' }} onPointerLeave={() => setMenu(null)}>
      <button className="block px-3 py-2 text-red-400" onClick={() => { const p = rows.find(p => p.id === menu.id); if (p) void remove(p); }}>删除仓位</button>
      <button className="block px-3 py-2 text-text-2" onClick={() => setMenu(null)}>取消</button>
    </div>}
    {editor && <div className="absolute inset-0 flex items-center justify-center bg-black/50" style={{ pointerEvents: 'auto' }}>
      <form role="dialog" aria-modal="true" aria-label={`修改${labels[editor.field]}`} className="w-[250px] max-w-full rounded border border-border bg-bg-1 p-3 text-xs text-text-1 shadow-xl"
        onKeyDown={e => { if (e.key === 'Escape') { e.stopPropagation(); setEditor(null); } }}
        onSubmit={e => {
          e.preventDefault(); const p = rows.find(p => p.id === editor.id); if (!p) { setEditor(null); return; }
          const blank = editor.value.trim() === '';
          const price = blank && editor.field === 'target_price' ? null : blank ? NaN : Number(editor.value);
          const next = { ...p,[editor.field]: price } as UserPosition;
          const error = validatePrices(next);
          if (error) { setEditor({ ...editor,error }); return; }
          positions.commit(next); setEditor(null);
        }}>
        <label className="block mb-2" htmlFor={`position-price-${paneIndex}`}>修改{labels[editor.field]}{editor.field === 'target_price' ? '（留空即不设目标）' : ''}</label>
        <input id={`position-price-${paneIndex}`} autoFocus type="number" step="any" className="w-full rounded border border-border bg-bg-2 px-2 py-2" value={editor.value} onFocus={e => e.target.select()} onChange={e => setEditor({ ...editor,value: e.target.value,error: '' })} />
        {editor.error && <div role="alert" className="my-2 text-red-400">{editor.error}</div>}
        <div className="mt-3 flex justify-end gap-2">
          <button type="button" onClick={() => setEditor(null)}>取消</button><button type="submit" className="rounded bg-accent px-3 py-1 text-white">保存</button>
        </div>
      </form>
    </div>}
  </div>;
}
