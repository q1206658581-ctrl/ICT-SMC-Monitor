import { useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { useChartStore, type Tf } from '../../../store';
import { positions, usePositions } from './store';
import type { PositionDraft } from './model';
export function DrawPositionButton({ alertId, decisionId }: { alertId: string | null; decisionId?: string }) {
  const [busy,setBusy] = useState(false);
  return <button type="button" className="whitespace-nowrap rounded border border-border px-2 py-1 text-text-1 hover:bg-bg-3 disabled:opacity-40" disabled={!alertId || busy}
    title="按告警的确定性入场区、失效位和第一目标生成可编辑仓位，不调用 AI"
    onClick={async event => {
      event.stopPropagation(); if (!alertId) return;
      setBusy(true);
      try {
        const draft = await invoke<PositionDraft>('get_user_position_draft', { alertId, decisionId: decisionId ?? null });
        const position = await positions.create(draft);
        const chart = useChartStore.getState();
        chart.setHighlightSmt(null);
        chart.setHighlightCandidate(null);
        chart.setHighlightAlert(null);
        chart.setSymbol(draft.symbol);
        if (!chart.symbols.includes(draft.symbol)) chart.setSingleMode(true);
        if (draft.drawn_tf) chart.setTf(draft.drawn_tf as Tf);
        usePositions.getState().setMode(null);
        usePositions.getState().select(position.id,null);
        toast.success('仓位已画到图上，可拖动标签或价位线调整');
      } catch (error) { toast.error(`无法预填仓位：${String(error)}`); }
      finally { setBusy(false); }
    }}>{busy ? '正在生成…' : '画到图上'}</button>;
}
