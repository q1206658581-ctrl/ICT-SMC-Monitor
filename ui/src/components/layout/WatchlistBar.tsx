import { WatchlistEditor } from './WatchlistEditor';
import { toast } from 'sonner';
import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useChartStore } from '../../store';
import type { Watchlist } from '../../types/watchlist';
import { Button } from '../ui/button';
import { Columns2, Square } from 'lucide-react';

export function WatchlistBar() {
  const [managerOpen, setManagerOpen] = useState(false);
  const [pendingSymbol, setPendingSymbol] = useState<string | undefined>();
  const [editing, setEditing] = useState<Watchlist | null | undefined>();
  const [revision, setRevision] = useState(0);
  const [watchlists, setWatchlists] = useState<Watchlist[]>([]);

  const activeWatchlist = useChartStore((s) => s.activeWatchlist);
  const setActiveWatchlist = useChartStore((s) => s.setActiveWatchlist);
  const singleMode = useChartStore((s) => s.singleMode);
  const setSingleMode = useChartStore((s) => s.setSingleMode);

  useEffect(() => {
    let cancelled = false;
    void invoke<Watchlist[]>('list_watchlists')
      .then((lists) => {
        if (!cancelled) setWatchlists(lists);
      })
      .catch((e: unknown) => console.warn('list_watchlists failed', e));
    return () => {
      cancelled = true;
    };
  }, [revision]);

  useEffect(() => {
    const manage = (e: Event) => {
      setPendingSymbol((e as CustomEvent<string>).detail || undefined);
      setEditing(undefined); setManagerOpen(true);
    };
    window.addEventListener('manage-groups', manage);
    return () => window.removeEventListener('manage-groups', manage);
  }, []);
  const saved = () => {
    setRevision((n) => n + 1);
    window.dispatchEvent(new Event('watchlists-changed'));
    setPendingSymbol(undefined);
    toast.success('分组已保存');
  };

  const selectWatchlist = async (wl: Watchlist) => {
    try {
      await invoke('set_viewing_group', { id: wl.id });
      setActiveWatchlist(wl);
    } catch (e) {
      toast.error(`切换分组失败：${String(e)}`);
    }
  };

  const activeId = activeWatchlist?.id;

  return (
    <>
    <div className="flex items-center gap-1 overflow-x-auto">
      {watchlists.map((wl) => (
        <button
          key={wl.id}
          onClick={() => selectWatchlist(wl)}
          className="flex items-center gap-1 px-2.5 py-1 rounded-md text-xs whitespace-nowrap transition-colors"
          style={{
            background: activeId === wl.id ? 'var(--bg-3)' : 'var(--bg-2)',
            color: activeId === wl.id ? 'var(--text-1)' : 'var(--text-2)',
            border: activeId === wl.id ? '1px solid var(--accent)' : '1px solid transparent',
          }}
        >
          {wl.name}
        </button>
      ))}

      {activeWatchlist && activeWatchlist.symbols.length > 1 && (
        <>
          <div className="w-px h-4 mx-1" style={{ background: 'var(--border)' }} />
          <Button
            size="sm"
            variant={singleMode ? 'default' : 'ghost'}
            onClick={() => setSingleMode(!singleMode)}
            title={singleMode ? '单 pane 模式' : '多 pane 模式'}
          >
            {singleMode ? <Square size={14} /> : <Columns2 size={14} />}
            {singleMode ? 'Single' : `×${activeWatchlist.symbols.length}`}
          </Button>
        </>
      )}
      <Button size="sm" variant="ghost" className="shrink-0" onClick={() => { setPendingSymbol(undefined); setManagerOpen(true); }}>管理分组</Button>
    </div>
    {managerOpen && editing === undefined && <div className="fixed inset-0 z-50 flex items-center justify-center" style={{ background: 'rgba(0,0,0,.55)' }}
      onClick={() => setManagerOpen(false)}>
      <div role="dialog" aria-label="管理分组" className="w-[520px] max-h-[80vh] overflow-auto bg-bg-2 border border-border rounded-lg p-5" onClick={(e) => e.stopPropagation()}>
        <div className="flex justify-between items-center mb-3"><strong>管理分组</strong><Button variant="ghost" size="sm" onClick={() => setManagerOpen(false)}>关闭</Button></div>
        {pendingSymbol && <p className="text-sm text-text-2 mb-3">将 {pendingSymbol} 加入现有分组或新建分组。每组最多 3 个品种。</p>}
        {watchlists.map((wl) => <div key={wl.id} className="flex items-center justify-between gap-2 py-3 border-b border-border">
          <div><div className="text-sm">{wl.name}</div><div className="text-xs text-text-3">{wl.symbols.join(' / ')}</div></div>
          <Button size="sm" variant="ghost" disabled={!!pendingSymbol && (wl.symbols.length >= 3 || wl.symbols.includes(pendingSymbol))}
            onClick={() => setEditing(pendingSymbol ? { ...wl, symbols: [...wl.symbols, pendingSymbol] } : wl)}>{pendingSymbol ? '加入' : '编辑'}</Button>
        </div>)}
        <Button className="mt-4" size="sm" onClick={() => setEditing(null)}>新建分组</Button>
      </div>
    </div>}
    {managerOpen && editing !== undefined && <WatchlistEditor editTarget={editing} initialSymbol={pendingSymbol}
      onClose={() => setEditing(undefined)} onSaved={saved} />}
    </>
  );
}
