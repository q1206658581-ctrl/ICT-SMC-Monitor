import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Input } from '../ui/input';

export type SearchSymbol = { symbol: string; description: string; exchange: string; kind: string };
const cache = new Map<string, { at: number; rows: SearchSymbol[] }>();
export function useSymbolSearch(query: string) {
  const [state, setState] = useState({ query: '', rows: [] as SearchSymbol[], loading: false, error: '' });
  const clean = query.trim();
  useEffect(() => {
    if (!clean) return;
    let cancelled = false;
    const timer = setTimeout(async () => {
      const cached = cache.get(clean.toUpperCase());
      if (cached && Date.now() - cached.at < 300_000) {
        setState({ query: clean, rows: cached.rows, loading: false, error: '' }); return;
      }
      setState({ query: clean, rows: [], loading: true, error: '' });
      try {
        const rows = await invoke<SearchSymbol[]>('search_symbols', { query: clean });
        if (cache.size > 50) cache.clear();
        cache.set(clean.toUpperCase(), { at: Date.now(), rows });
        if (!cancelled) setState({ query: clean, rows, loading: false, error: '' });
      } catch (e) {
        if (!cancelled) setState({ query: clean, rows: [], loading: false, error: String(e) });
      }
    }, 300);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [clean]);
  return state.query === clean ? state : { query: clean, rows: [], loading: !!clean, error: '' };
}

export function SymbolPicker({ value, onChange }: { value: string; onChange: (symbol: string) => void }) {
  const [query, setQuery] = useState(value);
  const [open, setOpen] = useState(false);
  const search = useSymbolSearch(open ? query : '');
  return <div className="relative flex-1 min-w-0">
    <Input value={open ? query : value} placeholder="搜索品种并选择数据源" aria-label="搜索分组品种"
      onFocus={() => { setQuery(value); setOpen(true); }} onChange={(e) => { setQuery(e.target.value); setOpen(true); }}
      onKeyDown={(e) => { if (e.key === 'Escape') setOpen(false); }} />
    {open && <div className="absolute left-0 right-0 top-full z-[60] max-h-48 overflow-auto rounded border border-border bg-bg-3 shadow-lg">
      <button className="text-xs text-text-3 p-2" onClick={() => setOpen(false)}>关闭搜索</button>
      {search.loading && <p className="p-2 text-xs">正在搜索 TradingView…</p>}
      {search.error && <p role="alert" className="p-2 text-xs text-red-400">{search.error}</p>}
      {!search.loading && !search.error && !search.rows.length && <p className="p-2 text-xs">输入名称或代码，选择搜索结果</p>}
      {search.rows.map((row) => <button key={row.symbol} className="block w-full text-left p-2 text-xs hover:bg-bg-2"
        onClick={() => { onChange(row.symbol); setQuery(row.symbol); setOpen(false); }}>
        <strong>{row.symbol}</strong><div className="text-text-2">{row.description}</div>
      </button>)}
    </div>}
  </div>;
}
