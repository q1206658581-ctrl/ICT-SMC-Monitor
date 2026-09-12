import { buildCorrelationPairs, confirmedCorrelations, type CorrelationDraft } from './correlationDraft';
import { SymbolPicker } from './SymbolSearch';
import { useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useChartStore } from '../../store';
import type { Watchlist, WatchlistInput, DefaultCorrelation } from '../../types/watchlist';
import { Button } from '../ui/button';
import { Input } from '../ui/input';

type Props = {
  onClose: () => void;
  editTarget?: Watchlist | null;
  onSaved: () => void;
  initialSymbol?: string;
};

function errorMessage(error: unknown): string {
  if (typeof error === 'string') return error;
  if (error instanceof Error) return error.message;
  return '保存失败';
}

export function WatchlistEditor({ onClose, editTarget, onSaved, initialSymbol }: Props) {
  const initialSymbols = editTarget?.symbols ?? [initialSymbol || ''];
  const [name, setName] = useState(editTarget?.name ?? '');
  const [symbols, setSymbols] = useState<string[]>(initialSymbols);
  const [correlations, setCorrelations] = useState<CorrelationDraft[]>(() =>
    buildCorrelationPairs(initialSymbols, editTarget?.correlations ?? []),
  );
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const correlationRequest = useRef(0);
  const activeWatchlist = useChartStore((state) => state.activeWatchlist);
  const setActiveWatchlist = useChartStore((state) => state.setActiveWatchlist);

  const updateSymbols = (next: string[]) => {
    setSymbols(next);
    ++correlationRequest.current;
    setCorrelations((prev) => buildCorrelationPairs(next, prev));
  };

  const setSymbolAt = (idx: number, val: string) => {
    const next = [...symbols];
    next[idx] = val;
    updateSymbols(next);
  };

  const addSymbol = () => {
    if (symbols.length < 3) updateSymbols([...symbols, '']);
  };

  const removeSymbol = (idx: number) => {
    updateSymbols(symbols.filter((_, i) => i !== idx));
  };

  const setCorrDirection = (a: string, b: string, direction: 'positive' | 'negative') => {
    ++correlationRequest.current;
    setCorrelations((prev) =>
      prev.map((c) => (c.a === a && c.b === b ? { ...c, direction } : c)),
    );
  };

  const handlePrefill = async (a: string, b: string) => {
    const request = ++correlationRequest.current;
    try {
      const result = await invoke<DefaultCorrelation>('default_correlation_cmd', {
        symbolA: a, symbolB: b,
      });
      if (correlationRequest.current === request) {
        if (!result.known) { setError('这两个品种没有内置关系，请手动选择正相关或负相关'); return; }
        setCorrDirection(a, b, result.direction);
      }
    } catch { /* ignore */ }
  };

  const save = async () => {
    if (saving) return;
    setError(null);
    const cleanSymbols = symbols.map((s) => s.trim()).filter(Boolean);
    if (cleanSymbols.length < 1 || cleanSymbols.length > 3) {
      setError('每组需要 1–3 个品种');
      return;
    }
    if (new Set(cleanSymbols).size !== cleanSymbols.length) { setError('同一分组不能重复添加品种'); return; }
    if (!name.trim()) { setError('请输入分组名称'); return; }
    let selected: WatchlistInput['correlations'];
    try { selected = confirmedCorrelations(buildCorrelationPairs(cleanSymbols, correlations)); }
    catch (e) { setError(errorMessage(e)); return; }
    setSaving(true);
    try {
      const input: WatchlistInput = { name: name.trim(), symbols: cleanSymbols, correlations: selected };
      if (editTarget) {
        const updated = await invoke<Watchlist>('update_watchlist', { id: editTarget.id, input });
        if (activeWatchlist?.id === editTarget.id) {
          await invoke('set_viewing_group', { id: updated.id });
          setActiveWatchlist(updated);
        }
      } else {
        await invoke('create_watchlist', { input });
      }
      onSaved();
      onClose();
    } catch (e: unknown) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center" style={{ background: 'rgba(0,0,0,0.5)' }} onClick={() => { if (!saving) onClose(); }}>
      <div
        className="w-[520px] max-h-[85vh] overflow-y-auto rounded-lg p-5 flex flex-col gap-4"
        style={{ background: 'var(--bg-2)', border: '1px solid var(--border-strong)' }}
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="text-base font-semibold text-text-1">
          {editTarget ? '编辑分组' : '新建分组'}
        </h2>

        <fieldset disabled={saving} className="contents">
        <div className="flex flex-col gap-1">
          <label className="text-xs text-text-3">名称</label>
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="EU / GU / DXY" />
        </div>

        <div className="flex flex-col gap-1">
          <label className="text-xs text-text-3">品种（1–3 个，搜索后选择数据源）</label>
          {symbols.map((sym, i) => (
            <div key={i} className="flex items-center gap-2">
              <SymbolPicker value={sym} onChange={(value) => setSymbolAt(i, value)} />
              {symbols.length > 1 && (
                <Button size="icon" variant="ghost" onClick={() => removeSymbol(i)}>✕</Button>
              )}
            </div>
          ))}
          {symbols.length < 3 && (
            <Button size="sm" variant="ghost" onClick={addSymbol}>+ 添加品种</Button>
          )}

        </div>

        {correlations.length > 0 && (
          <div className="flex flex-col gap-1">
            <label className="text-xs text-text-3">品种关系（请确认后保存）</label><p className="text-xs text-text-3">正相关：通常同向；负相关：通常反向。此设置决定 SMT 比较方式，不会自动推断新品种的策略关系。</p>
            {correlations.map((c) => (
              <div key={`${c.a}-${c.b}`} className="flex items-center gap-2 text-xs">
                <span className="text-text-2">{c.a.replace(/.*:/, '')} ↔ {c.b.replace(/.*:/, '')}</span>
                <select
                  value={c.direction}
                  onChange={(e) => setCorrDirection(c.a, c.b, e.target.value as 'positive' | 'negative')}
                  className="bg-bg-3 text-text-1 rounded px-1 py-0.5"
                >
                  <option value="" disabled>请选择关系</option>
                  <option value="positive">正相关</option>
                  <option value="negative">负相关</option>
                </select>
                <Button size="sm" variant="ghost" onClick={() => handlePrefill(c.a, c.b)}>使用预设</Button>
              </div>
            ))}
          </div>
        )}

        </fieldset>
        {error && <p className="text-sm text-red-400">{error}</p>}

        <div className="flex justify-end gap-2">
          <Button size="sm" variant="ghost" onClick={() => { if (!saving) onClose(); }}>取消</Button>
          <Button size="sm" onClick={save} disabled={saving}>{saving ? '保存中...' : '保存'}</Button>
        </div>
      </div>
    </div>
  );
}
