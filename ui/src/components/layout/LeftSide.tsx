import { toast } from 'sonner';
import { useSymbolSearch } from './SymbolSearch';
import { useEffect, useState } from 'react';
import { useSymbolDrag } from './useSymbolDrag';
import { GripVertical, Search, X } from 'lucide-react';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { useChartStore } from '../../store';
import { orderedSymbols, useSymbolOrderStore } from '../../store/symbolOrder';
import { Input } from '../ui/input';
import type { BarPayload, SymbolMeta } from '../../types/ipc';

function displaySymbol(symbol: string): string {
  return symbol.replace('OANDA:', '').replace('COINBASE:', '');
}

function pricePrecision(symbol: string): number {
  // Match ChartPane's display precision; stored quotes retain their full value.
  if (symbol.includes('DXY')) return 3;
  if (symbol.includes('BTC') || symbol.includes('XAU') || symbol.includes('XAG')) return 2;
  return 5;
}

export function LeftSide() {
  const [query, setQuery] = useState('');
  const [adding, setAdding] = useState<string | null>(null);
  const search = useSymbolSearch(query);
  const symbol = useChartStore((s) => s.symbol);
  const lastClose = useChartStore((s) => s.lastClose);
  const [symbols, setSymbols] = useState<SymbolMeta[]>([]);
  const [prices, setPrices] = useState<Record<string, number>>({});
  const order = useSymbolOrderStore((s) => s.order);
  const setOrder = useSymbolOrderStore((s) => s.setOrder);
  const [reorderNotice, setReorderNotice] = useState('');

  useEffect(() => {
    let cancelled = false;
    const reload = () => invoke<SymbolMeta[]>('list_symbols')
      .then((rows) => {
        if (!cancelled) setSymbols(rows);
      })
      .catch((e) => console.warn('list_symbols failed', e));
    void reload();
    window.addEventListener('watchlists-changed', reload);
    // Fetch last known prices from SQLite so all symbols show a price
    // even if they are not in the actively subscribed watchlist.
    invoke<[string, number | null][]>('list_symbol_prices')
      .then((rows) => {
        if (!cancelled) {
          const map: Record<string, number> = {};
          for (const [sym, price] of rows) {
            if (price != null) map[sym] = price;
          }
          setPrices(map);
        }
      })
      .catch((e) => console.warn('list_symbol_prices failed', e));
    return () => { cancelled = true; window.removeEventListener('watchlists-changed', reload); };
  }, []);

  useEffect(() => {
    let cancelled = false;
    const ps: Promise<() => void>[] = [];
    const sub = (topic: string) =>
      listen<BarPayload>(topic, (e) => {
        if (cancelled) return;
        if (e.payload.tf !== '1m') return;
        setPrices((prev) => ({ ...prev, [e.payload.symbol]: e.payload.close }));
      });
    ps.push(sub('bar:update'), sub('bar:closed'));
    return () => {
      cancelled = true;
      ps.forEach((p) => p.then((u) => u()));
    };
  }, []);

  const rows = orderedSymbols(symbols.length ? symbols : [{ symbol, provider: 'tradingview' }], order);
  const canReorder = !query.trim() && !adding && rows.length > 1;
  const moveSymbol = (source: string, target: string, after: boolean) => {
    if (source === target || !rows.some((row) => row.symbol === source)) return;
    const next = rows.map((row) => row.symbol).filter((symbol) => symbol !== source);
    const index = next.indexOf(target);
    if (index < 0) return;
    next.splice(index + (after ? 1 : 0), 0, source);
    setOrder(next);
    setReorderNotice(`${displaySymbol(source)} 已移到第 ${next.indexOf(source) + 1} 位`);
  };
  const drag = useSymbolDrag(canReorder, moveSymbol);

  const filtered = rows.filter((r) => r.symbol.toLowerCase().includes(query.trim().toLowerCase()));
  const selectSymbol = async (target: string, group = false) => {
    setAdding(target);
    try {
      await invoke('add_symbol', { symbol: target });
      setSymbols(await invoke<SymbolMeta[]>('list_symbols'));
      if (group) {
        window.dispatchEvent(new CustomEvent('manage-groups', { detail: target }));
      } else {
        // Explicit sidebar selection may intentionally be outside the active group.
        useChartStore.setState({ symbol: target, singleMode: true,
          highlightSmtId: null, highlightCandidateId: null, highlightAlertId: null });
      }
      setQuery('');
    } catch (e) { toast.error(String(e)); }
    finally { setAdding(null); }
  };

  return (
    <div
      className="h-full flex flex-col"
      style={{
        background: 'var(--bg-1)',
        borderRight: '1px solid var(--border)',
      }}
    >
      <div className="p-3 border-b" style={{ borderColor: 'var(--border)' }}>
        <div className="relative">
          <Search
            size={14}
            className="absolute left-2 top-1/2 -translate-y-1/2"
            style={{ color: 'var(--text-3)' }}
          />
          <Input placeholder="搜索品种…" aria-label="搜索品种" className="pl-7 pr-7" value={query} onChange={(e) => setQuery(e.target.value)} />
          {query && <button aria-label="清空搜索" className="absolute right-2 top-1/2 -translate-y-1/2" onClick={() => setQuery('')}><X size={13} /></button>}
        </div>
      </div>
      <ul ref={drag.listRef} className="flex-1 min-h-0 overflow-auto py-1" aria-label="品种列表">
        {filtered.map((row, index) => {
          const active = row.symbol === symbol;
          const price = prices[row.symbol] ?? (active ? lastClose : null);
          return (
            <li
              key={row.symbol}
              data-sort-symbol={row.symbol}
              className="relative flex items-center select-none"
              style={{ background: active ? 'var(--bg-2)' : 'transparent', opacity: drag.dragging === row.symbol ? 0.5 : 1 }}
            >
              {drag.dropTarget?.symbol === row.symbol && <div aria-hidden="true" className={`absolute inset-x-1 h-0.5 bg-accent pointer-events-none z-10 ${drag.dropTarget.after ? 'bottom-0' : 'top-0'}`} />}
              <button
                type="button"
                className="shrink-0 px-1 py-2 touch-none text-text-3 hover:text-text-1 cursor-grab active:cursor-grabbing disabled:opacity-30 disabled:cursor-default focus-visible:outline focus-visible:outline-1 focus-visible:outline-accent"
                aria-label={`调整 ${displaySymbol(row.symbol)} 的顺序`}
                title={query.trim() ? '清空搜索后可拖动排序' : '按住拖动调整顺序；也可按 Alt + ↑ / ↓ 移动'}
                disabled={!canReorder}
                draggable={false}
                onDragStart={(e) => e.preventDefault()}
                onPointerDown={(e) => drag.start(e, row.symbol)}
                onPointerMove={drag.track}
                onPointerUp={drag.finish}
                onPointerCancel={drag.cancel}
                onLostPointerCapture={drag.cancel}
                onKeyDown={(e) => {
                  if (!e.altKey || !['ArrowUp', 'ArrowDown'].includes(e.key)) return;
                  e.preventDefault();
                  const after = e.key === 'ArrowDown';
                  const neighbor = rows[index + (after ? 1 : -1)];
                  if (neighbor) moveSymbol(row.symbol, neighbor.symbol, after);
                }}
              ><GripVertical size={13} /></button>
              <button type="button" className="min-w-0 flex-1 flex items-center justify-between gap-2 pr-3 py-2 text-left focus-visible:outline focus-visible:outline-1 focus-visible:outline-accent"
                title={row.symbol} disabled={!!adding}
                onClick={() => { void selectSymbol(row.symbol); }}
              >
                <span className="text-sm text-text-1 truncate">{displaySymbol(row.symbol)}</span>
                <span className="text-sm font-mono text-text-2 shrink-0">{price != null ? price.toFixed(pricePrecision(row.symbol)) : '—'}</span>
              </button>
            </li>
          );
        })}
        {!!query.trim() && <li className="border-t border-border mt-2 p-2 text-xs">
          {!filtered.length && <p className="text-text-3 pb-2">当前列表无匹配品种</p>}
          <p className="text-text-2 pb-2">TradingView · 请选择数据源</p>
          {search.loading && <p>正在搜索…</p>}
          {search.error && <p role="alert" className="text-red-400">{search.error}</p>}
          {!search.loading && !search.error && !search.rows.length && <p>未找到品种，请换个代码或名称</p>}
          {search.rows.map((row) => <div key={row.symbol} className="py-2 border-b border-border">
            <div className="font-medium text-text-1 break-all">{row.symbol}</div>
            <div className="text-text-3 leading-4 my-1">{row.description} · {row.kind}</div>
            <div className="flex gap-3 text-accent">
              <button disabled={!!adding} onClick={() => void selectSymbol(row.symbol)}>{adding === row.symbol ? '添加中…' : '仅看图'}</button>
              <button disabled={!!adding} onClick={() => void selectSymbol(row.symbol, true)}>加入分组</button>
            </div>
          </div>)}
        </li>}
      </ul>
      <span role="status" className="sr-only">{reorderNotice}</span>
      <div
        className="px-3 py-2 text-xs text-text-3"
        style={{ borderTop: '1px solid var(--border)' }}
      >
        选择品种查看单图；加入分组后参与该组监控
      </div>
    </div>
  );
}
