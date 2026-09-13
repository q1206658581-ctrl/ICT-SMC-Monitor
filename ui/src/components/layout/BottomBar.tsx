import { DrawPositionButton } from '../chart/positions/DrawPositionButton';
import { LogsPanel } from "./LogsPanel";
import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '../ui/tabs';
import { useChartStore, useLayoutStore, type Tf } from '../../store';
import type { SmtDivergence } from '../../types/structures';
import type { AlertRecord } from '../../types/alert';
import type { LlmDecisionListItem, LlmDecisionStatus } from '../../types/decision';
import { historyLimitForTf } from '../chart/historyWindow';
import { formatSmtInvalidationReasons } from '../../lib/smtInvalidation';

function ticker(sym: string) {
  return sym.split(':').pop() ?? sym;
}

const DISPLAY_TZ = 'Asia/Shanghai';

function fmtTime(ts: number | null | undefined) {
  if (!ts) return '-';
  const d = new Date(ts);
  return d.toLocaleString('zh-CN', {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
    timeZone: DISPLAY_TZ,
  });
}

function inverseDirection(direction: 'bullish' | 'bearish') {
  return direction === 'bullish' ? 'bearish' : 'bullish';
}

function groupLabel(id: string) {
  switch (id) {
    case 'eu-gu-dxy': return 'EU/GU';
    case 'aud-nzd-dxy': return 'AUD/NZD';
    case 'chf-cad-dxy': return 'CHF/CAD';
    default: return id || '-';
  }
}

function tradeDirection(direction: 'bullish' | 'bearish', sweeper: string, symbols: string[], watchlistId: string) {
  // EU/GU and AUD/NZD are negatively correlated with DXY, while CHF/CAD
  // are the positive-correlation group. Keep the persisted DXY structural
  // direction immutable and translate only the user-facing trade direction.
  return watchlistId !== 'chf-cad-dxy'
    && ticker(sweeper) === 'DXY'
    && symbols.some((symbol) => symbol !== sweeper)
    ? inverseDirection(direction)
    : direction;
}

function tfDurationMs(tf: string) {
  const durations: Record<string, number> = {
    '5m': 5 * 60_000,
    '30m': 30 * 60_000,
    '1h': 60 * 60_000,
  };
  return durations[tf] ?? 0;
}

async function loadCutoff(symbols: string[], tf: string) {
  const limit = historyLimitForTf(tf);
  // Inbox polling is intentionally local-only. Calling get_history here used
  // to trigger provider backfills every five seconds and starve real chart
  // timeframe switches behind the single TradingView history connection.
  try {
    return await invoke<number>('get_history_cutoff', { symbols, tf, limit });
  } catch (error) {
    // A visibility cutoff is optional metadata. If SQLite is briefly busy (or
    // a newly built UI is paired with an older backend), showing the complete
    // Inbox is safer than letting Promise.all suppress every table refresh.
    console.warn(`history cutoff unavailable for ${tf}; showing all Inbox rows`, error);
    return 0;
  }
}

function smtState(s: SmtDivergence) {
  return s.chains.find((chain) => chain.symbol === s.sweeper_symbol)?.detection_state
    ?? 'invalidated';
}

function smtInvalidationReason(s: SmtDivergence) {
  if (smtState(s) !== 'invalidated') return '-';
  return formatSmtInvalidationReasons(s.invalidation_reasons, s.sweeper_symbol);
}

function candidateStatusLabel(status: string) {
  switch (status) {
    case 'c2_confirmed': return 'Waiting';
    case 'validated': return 'Validated';
    case 're_check': return 'Re-check';
    case 'expired': return 'Expired';
    case 'invalidated': return 'Invalidated';
    default: return status;
  }
}

function candidateInvalidationReasonLabel(reason: AlertRecord['invalidation_reason']) {
  switch (reason) {
    case 'reverse': return '出现反向 LTF 结构';
    case 'c2_break': return '收盘突破 C2 失效边界';
    case 'smt_invalidated': return '源 SMT 后续失效';
    case 'ttl': return '时间窗口结束';
    default: return '该交易品种结构已失效';
  }
}

function compareAlertIdentity(a: AlertRecord, b: AlertRecord) {
  return a.watchlist_id.localeCompare(b.watchlist_id)
    || (a.validation_symbol ?? '').localeCompare(b.validation_symbol ?? '')
    || a.id.localeCompare(b.id);
}

function compareSetupAlerts(a: AlertRecord, b: AlertRecord) {
  return b.created_at - a.created_at || compareAlertIdentity(a, b);
}

function compareReversalAlerts(a: AlertRecord, b: AlertRecord) {
  return (b.validation_ts ?? 0) - (a.validation_ts ?? 0)
    || b.created_at - a.created_at
    || compareAlertIdentity(a, b);
}

function SmtInbox() {
  const [items, setItems] = useState<SmtDivergence[]>([]);
  const [cutoffs, setCutoffs] = useState<Record<string, number>>({});
  const [loadError, setLoadError] = useState<string | null>(null);
  const openSmt = useChartStore((s) => s.openSmt);
  const highlightSmtId = useChartStore((s) => s.highlightSmtId);
  const symbols = useChartStore((s) => s.symbols);
  const watchlistId = useChartStore((s) => s.activeWatchlist?.id);

  useEffect(() => {
    const poll = async () => {
      try {
        const list = await invoke<SmtDivergence[]>('list_smt', { watchlistId });
        const [m30Cutoff, h1Cutoff] = await Promise.all([
          loadCutoff(symbols, '30m'),
          loadCutoff(symbols, '1h'),
        ]);
        setItems(list);
        setCutoffs({ '30m': m30Cutoff, '1h': h1Cutoff });
        setLoadError(null);
      } catch (e) {
        setLoadError(String(e));
      }
    };
    void poll();
    const id = setInterval(poll, 5000);
    return () => clearInterval(id);
  }, [symbols, watchlistId]);

  if (loadError) {
    return <div className="px-3 py-2 text-sm text-red-400">SMT 加载失败：{loadError}</div>;
  }

  if (items.length === 0) {
    return <div className="px-3 py-2 text-sm text-text-3">暂无 SMT 历史记录</div>;
  }

  const inRangeItems = items.filter((s) => {
    const chain = s.chains.find((item) => item.symbol === s.sweeper_symbol);
    const liquidity = s.liquidity_refs.find((item) => item.symbol === s.sweeper_symbol);
    const cutoff = cutoffs[s.comparison_timeframe] ?? 0;
    if (!chain || !liquidity) return false;
    // Inbox visibility is a chart-audit contract, not just an SMT-K recency
    // check. The white line starts at the swept liquidity reference, which
    // can be much older than SMT K; hide a row when that endpoint is outside
    // the common 1000-bar history of the three panes.
    const evidenceStart = Math.min(chain.c1_candle.ts, liquidity.ref_ts);
    return evidenceStart >= cutoff;
  });
  const visibleItems = inRangeItems;

  const stateLabel = (s: string) => {
    switch (s) {
      case 'smt_k_detected': return 'SMT K';
      case 'c2_confirmed': return 'C2 ✓';
      case 'c3_entry': return 'C3';
      case 'invalidated': return '✗';
      default: return s;
    }
  };

  return (
    <div className="absolute inset-0 overflow-auto">
      <table className="w-full text-xs">
        <thead className="sticky top-0" style={{ background: 'var(--bg-1)' }}>
          <tr className="text-text-3">
           <th className="px-3 py-1 text-left">SMT K时间</th>
           <th className="px-3 py-1 text-left">组</th>
           <th className="px-3 py-1 text-left">C2确认时间</th>
            <th className="px-3 py-1 text-left">MTF</th>
           <th className="px-3 py-1 text-left">交易方向</th>
           <th className="px-3 py-1 text-left">Sweeper</th>
           <th className="px-3 py-1 text-left">Trade</th>
           <th className="px-3 py-1 text-left">C2类别</th>
           <th className="px-3 py-1 text-left">状态</th>
           <th className="px-3 py-1 text-left">失效原因</th>
           <th className="px-3 py-1 text-left">强弱</th>
          </tr>
        </thead>
        <tbody>
          {visibleItems
            .slice()
            .sort((a, b) => {
      const ta = a.chains.find(c => c.symbol === a.sweeper_symbol)?.smt_k_candle.ts ?? 0;
      const tb = b.chains.find(c => c.symbol === b.sweeper_symbol)?.smt_k_candle.ts ?? 0;
      return tb - ta;
    })
            .map((s) => (
              <tr
                key={s.id}
                className={`${smtState(s) === 'invalidated' && highlightSmtId !== s.id ? 'opacity-55' : ''} cursor-pointer hover:bg-bg-2`}
                style={highlightSmtId === s.id
                  ? { background: 'rgba(255, 235, 59, 0.12)', boxShadow: 'inset 3px 0 0 rgba(255, 235, 59, 0.9)' }
                  : undefined}
                aria-selected={highlightSmtId === s.id}
                title={smtState(s) === 'invalidated'
                  ? `${smtInvalidationReason(s)}；点击仍可回看形成区域`
                  : undefined}
                onClick={() => {
                  openSmt({
                    id: s.id,
                    tf: s.comparison_timeframe as Tf,
                    symbol: s.trade_symbols[0] ?? s.sweeper_symbol,
                  });
                }}
              >
               <td className="px-3 py-1 text-text-2">{fmtTime(s.chains.find(c => c.symbol === s.sweeper_symbol)?.smt_k_candle.ts)}</td>
               <td className="px-3 py-1 text-text-2">{groupLabel(s.watchlist_id)}</td>
               <td className="px-3 py-1 text-text-2">{(() => {
                 const c2Ts = s.chains.find(c => c.symbol === s.sweeper_symbol)?.c2_candle?.ts;
                 return c2Ts == null ? '-' : fmtTime(c2Ts + tfDurationMs(s.comparison_timeframe));
               })()}</td>
               <td className="px-3 py-1 text-text-2">{s.comparison_timeframe}</td>
             <td className="px-3 py-1" style={{ color: tradeDirection(s.candidate_direction, s.sweeper_symbol, s.symbol_set, s.watchlist_id) === 'bullish' ? 'var(--bull)' : 'var(--bear)' }}>
                {tradeDirection(s.candidate_direction, s.sweeper_symbol, s.symbol_set, s.watchlist_id) === 'bullish' ? '↑ Bull' : '↓ Bear'}
                </td>
                <td className="px-3 py-1 text-text-2">{ticker(s.sweeper_symbol)}</td>
                <td className="px-3 py-1 text-text-2">{s.trade_symbols.map(ticker).join(" / ")}</td>
                <td className="px-3 py-1 text-text-2">
                  {s.chains.map((c) => (
                    <div key={c.symbol}>{ticker(c.symbol)}:{c.c2_case ?? '-'}</div>
                  ))}
                </td>
                <td className="px-3 py-1 text-text-2">
                  {stateLabel(smtState(s))}
                </td>
                <td className="px-3 py-1 text-text-2 max-w-56 whitespace-normal">
                  {smtInvalidationReason(s)}
                </td>
                <td className="px-3 py-1 text-text-2">
                  {s.strength.map((st) => `${ticker(st.symbol)}:${st.label}`).join(' ')}
                </td>
              </tr>
            ))}
        </tbody>
      </table>
    </div>
  );
}

function SetupAlertInbox() {
  const [items, setItems] = useState<AlertRecord[]>([]);
  const [cutoff5m, setCutoff5m] = useState(0);
  const setTf = useChartStore((s) => s.setTf);
  const setSymbol = useChartStore((s) => s.setSymbol);
  const requestReload = useChartStore((s) => s.requestStructuresReload);
  const candidateValidatedOnly = useChartStore((s) => s.candidateValidatedOnly);
  const alertEnabled = useChartStore((s) => s.alertEnabled);
  const setHighlightAlert = useChartStore((s) => s.setHighlightAlert);
  const symbols = useChartStore((s) => s.symbols);
  const watchlistId = useChartStore((s) => s.activeWatchlist?.id);

  useEffect(() => {
    const poll = async () => {
      try {
        const [list, bars] = await Promise.all([
          invoke<AlertRecord[]>('list_alerts', { limit: null, watchlistId }),
          loadCutoff(symbols, '5m'),
        ]);
        setItems(list);
        setCutoff5m(bars);
      } catch { /* ignore */ }
    };
    void poll();
    const id = setInterval(poll, 5000);
    return () => clearInterval(id);
  }, [symbols, watchlistId]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen<AlertRecord>('ict:alert:fired', async () => {
      try {
        setItems(await invoke<AlertRecord[]>('list_alerts', { limit: null, watchlistId }));
      } catch { /* ignore */ }
    }).then((value) => { unlisten = value; });
    return () => { unlisten?.(); };
  }, [watchlistId]);

  if (items.length === 0) {
    return <div className="px-3 py-2 text-sm text-text-3">{alertEnabled ? '暂无 C2 警报' : '新警报已关闭；暂无历史 C2 警报'}</div>;
  }

  const inRangeItems = items.filter((alert) => alert.c2_candle_ts >= cutoff5m);
  const filtered = candidateValidatedOnly
    ? inRangeItems.filter((alert) => alert.validation_ts !== null)
    : inRangeItems;

  if (filtered.length === 0) {
    return (
      <div className="px-3 py-2 text-sm text-text-3">
        {candidateValidatedOnly
          ? `当前可定位范围有 ${inRangeItems.length} 条 C2 警报，但没有已确认反转的记录。`
          : '当前时间范围内暂无 C2 警报'}
      </div>
    );
  }

  return (
    <div className="absolute inset-0 overflow-auto">
      <table className="w-full text-xs">
        <thead className="sticky top-0" style={{ background: 'var(--bg-1)' }}>
          <tr>
           <th className="px-3 py-1 text-left">警报时间</th>
           <th className="px-3 py-1 text-left">组</th>
           <th className="px-3 py-1 text-left">交易品种</th>
           <th className="px-3 py-1 text-left">SMT K时间</th>
           <th className="px-3 py-1 text-left">链路</th>
           <th className="px-3 py-1 text-left">交易方向</th>
           <th className="px-3 py-1 text-left">C2确认时间</th>
           <th className="px-3 py-1 text-left">C2类型</th>
           <th className="px-3 py-1 text-left">C3</th>
            <th className="px-3 py-1 text-left">反转</th>
           <th className="px-3 py-1 text-left">状态</th>
           <th className="px-3 py-1 text-left">失效原因</th>
           <th className="px-3 py-1 text-left">评分</th>
           <th className="px-3 py-1 text-left">仓位</th>
          </tr>
        </thead>
        <tbody>
          {filtered
            .slice()
            .sort(compareSetupAlerts)
            .map((alert) => {
              const symbol = alert.validation_symbol ?? alert.trade_symbols[0];
              const invalidated = alert.setup_status === 'invalidated';
              const invalidationReason = invalidated
                ? candidateInvalidationReasonLabel(alert.invalidation_reason)
                : '-';
              return (
              <tr
                key={alert.id}
                className={!symbol
                  ? 'cursor-not-allowed opacity-50'
                  : invalidated
                    ? 'cursor-pointer opacity-60 hover:bg-bg-2'
                    : 'cursor-pointer hover:bg-bg-2'}
                title={invalidated ? `已失效：${invalidationReason}；点击可复盘` : undefined}
                onClick={() => {
                  if (!symbol) return;
                  setSymbol(symbol);
                  setTf(alert.validation_timeframe as Tf);
                  requestReload();
                  setHighlightAlert(alert.id);
                }}
              >
               <td className="px-3 py-1 text-text-2">{fmtTime(alert.created_at)}</td>
               <td className="px-3 py-1 text-text-2">{groupLabel(alert.watchlist_id)}</td>
               <td className="px-3 py-1 text-text-2">{ticker(symbol ?? '')}</td>
               <td className="px-3 py-1 text-text-2">{fmtTime(alert.smt_k_candle_ts)}</td>
               <td className="px-3 py-1 text-text-2">{`${alert.context_timeframe}-${alert.comparison_timeframe}-${alert.validation_timeframe}`}</td>
                <td
                  className="px-3 py-1"
                  style={{ color: alert.candidate_direction === 'bullish' ? 'var(--bull)' : 'var(--bear)' }}
                >
                  {alert.candidate_direction === 'bullish' ? '↑ Bull' : '↓ Bear'}
                </td>
                <td className="px-3 py-1 text-text-2">{fmtTime(alert.c2_candle_ts + tfDurationMs(alert.comparison_timeframe))}</td>
                <td className="px-3 py-1 text-text-2">{alert.c2_case ? `Case ${alert.c2_case}` : '-'}</td>
                <td className="px-3 py-1 text-text-2">{alert.c3_candle_ts !== null ? '✓' : '-'}</td>
                <td className="px-3 py-1 text-text-2">
                  {alert.validation_kind && alert.validation_ts
                    ? `${ticker(symbol ?? '')} ${alert.validation_kind.toUpperCase()} ${fmtTime(alert.validation_ts)}`
                    : '等待'}
                </td>
                <td className="px-3 py-1 text-text-2">{candidateStatusLabel(alert.setup_status)}</td>
                <td className="px-3 py-1 text-text-2">{invalidationReason}</td>
                <td className="px-3 py-1 text-text-2">{alert.deterministic_score.toFixed(2)}</td>
                <td className="px-3 py-1"><DrawPositionButton alertId={alert.id} /></td>
              </tr>
              );
            })}
        </tbody>
      </table>
    </div>
  );
}

function ReversalInbox() {
  const [items, setItems] = useState<AlertRecord[]>([]);
  const [cutoff5m, setCutoff5m] = useState(0);
  const setTf = useChartStore((s) => s.setTf);
  const setSymbol = useChartStore((s) => s.setSymbol);
  const requestReload = useChartStore((s) => s.requestStructuresReload);
  const setHighlightAlert = useChartStore((s) => s.setHighlightAlert);
  const symbols = useChartStore((s) => s.symbols);
  const watchlistId = useChartStore((s) => s.activeWatchlist?.id);

  useEffect(() => {
    const poll = async () => {
      try {
        const [alertList, bars] = await Promise.all([
          invoke<AlertRecord[]>('list_reversals', { limit: null, watchlistId }),
          loadCutoff(symbols, '5m'),
        ]);
        setItems(alertList);
        setCutoff5m(bars);
      } catch { /* ignore */ }
    };
    void poll();
    const id = setInterval(poll, 5000);
    return () => clearInterval(id);
  }, [symbols, watchlistId]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    (async () => {
      try {
        unlisten = await listen<AlertRecord>('ict:reversal:recorded', () => {
          (async () => {
            try {
              const list = await invoke<AlertRecord[]>('list_reversals', { limit: null, watchlistId });
              setItems(list);
            } catch { /* ignore */ }
          })();
        });
      } catch { /* not in tauri context */ }
    })();
    return () => { unlisten?.(); };
  }, [watchlistId]);

  const filteredAlerts = items.filter((a) => (a.validation_ts ?? 0) >= cutoff5m);

  if (items.length === 0) {
    return <div className="px-3 py-2 text-sm text-text-3">暂无反转确认记录</div>;
  }

  if (filteredAlerts.length === 0) {
    return <div className="px-3 py-2 text-sm text-text-3">暂无反转确认记录</div>;
  }

  return (
    <div className="absolute inset-0 overflow-auto">
      <table className="w-full text-xs">
        <thead className="sticky top-0" style={{ background: 'var(--bg-1)' }}>
          <tr className="text-text-3">
           <th className="px-3 py-1 text-left">反转触发时间</th>
           <th className="px-3 py-1 text-left">组</th>
           <th className="px-3 py-1 text-left">记录时间</th>
           <th className="px-3 py-1 text-left">验证品种</th>
           <th className="px-3 py-1 text-left">链路</th>
           <th className="px-3 py-1 text-left">LTF方向</th>
           <th className="px-3 py-1 text-left">反转类型</th>
           <th className="px-3 py-1 text-left">SMT K时间</th>
           <th className="px-3 py-1 text-left">评分</th>
           <th className="px-3 py-1 text-left">阶段</th>
           <th className="px-3 py-1 text-left">当前结果</th>
          </tr>
        </thead>
        <tbody>
          {filteredAlerts.slice().sort(compareReversalAlerts).map((a) => {
            const hasValidation = a.validation_symbol !== null
              && a.validation_ts !== null
              && a.validation_kind !== null;
            return (
              <tr
                key={a.id}
                className={hasValidation
                  ? 'cursor-pointer hover:bg-bg-2'
                  : 'cursor-not-allowed opacity-50'}
                title={hasValidation ? undefined : '告警缺少验证字段，无法定位'}
                onClick={() => {
                  if (!hasValidation) return;
                  setSymbol(a.validation_symbol!);
                  setTf(a.validation_timeframe as Tf);
                  requestReload();
                  setHighlightAlert(a.id);
                }}
              >
              <td className="px-3 py-1 text-text-2">{a.validation_ts ? fmtTime(a.validation_ts) : '-'}</td>
               <td className="px-3 py-1 text-text-2">{groupLabel(a.watchlist_id)}</td>
               <td className="px-3 py-1 text-text-2">{fmtTime(a.created_at)}</td>
               <td className="px-3 py-1 text-text-2">{ticker(a.validation_symbol ?? '')}</td>
               <td className="px-3 py-1 text-text-2">{`${a.context_timeframe}-${a.comparison_timeframe}-${a.validation_timeframe}`}</td>
              <td
                className="px-3 py-1"
                style={{ color: a.validation_direction === 'bullish' ? 'var(--bull)' : a.validation_direction === 'bearish' ? 'var(--bear)' : undefined }}
              >
                {a.validation_direction === 'bullish' ? '↑ Bull' : a.validation_direction === 'bearish' ? '↓ Bear' : '-'}
             </td>
              <td className="px-3 py-1 text-text-2">{a.validation_kind?.toUpperCase() ?? '-'}</td>
              <td className="px-3 py-1 text-text-2">{fmtTime(a.smt_k_candle_ts)}</td>
              <td className="px-3 py-1 text-text-2">{a.deterministic_score.toFixed(2)}</td>
              <td className="px-3 py-1 text-text-2">
                {a.c3_candle_ts !== null && (a.validation_ts ?? 0) >= a.c3_candle_ts ? 'C3' : 'C2'} · Case {a.c2_case || '-'}
              </td>
              <td className="px-3 py-1 text-text-2">{candidateStatusLabel(a.setup_status)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function decisionStatusLabel(status: LlmDecisionStatus) {
  switch (status) {
    case 'pending': return '调用中';
    case 'approved': return '通过';
    case 'rejected': return '拒绝';
    case 'error': return '错误';
  }
}

function DecisionInbox() {
  const [items, setItems] = useState<LlmDecisionListItem[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const watchlistId = useChartStore((s) => s.activeWatchlist?.id);
  const setTf = useChartStore((s) => s.setTf);
  const setSymbol = useChartStore((s) => s.setSymbol);
  const requestReload = useChartStore((s) => s.requestStructuresReload);
  const setHighlightAlert = useChartStore((s) => s.setHighlightAlert);

  useEffect(() => {
    let active = true;
    const poll = async () => {
      if (!watchlistId) {
        if (active) setItems([]);
        return;
      }
      try {
        const list = await invoke<LlmDecisionListItem[]>('list_llm_decision_items', {
          watchlistId,
          limit: 500,
        });
        if (!active) return;
        setItems(list);
        setLoadError(null);
      } catch (error) {
        if (active) setLoadError(String(error));
      }
    };
    void poll();
    const id = setInterval(poll, 5000);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, [watchlistId]);

  const selected = items.find((item) => item.id === selectedId) ?? null;
  const openContext = (item: LlmDecisionListItem) => {
    if (!item.alert_id || !item.trade_symbol || !item.validation_timeframe) return;
    setSymbol(item.trade_symbol);
    setTf(item.validation_timeframe);
    requestReload();
    setHighlightAlert(item.alert_id);
  };

  return (
    <div className="absolute inset-0 flex flex-col overflow-hidden">
      <div
        className="shrink-0 px-3 py-1 text-xs"
        style={{ color: '#f5c451', background: 'rgba(245, 158, 11, 0.10)', borderBottom: '1px solid rgba(245, 158, 11, 0.35)' }}
      >
        LLM 产出，非市场事实；仅供辅助复核，不替代人工交易判断。
      </div>
      {loadError ? (
        <div className="px-3 py-2 text-sm text-red-400">LLM 决策加载失败：{loadError}</div>
      ) : items.length === 0 ? (
        <div className="px-3 py-2 text-sm text-text-3">暂无 LLM 决策记录</div>
      ) : (
        <div className="flex min-h-0 flex-1">
          <div className={selected ? 'min-w-0 flex-[3] overflow-auto' : 'min-w-0 flex-1 overflow-auto'}>
            <table className="w-full text-xs">
              <thead className="sticky top-0 z-10" style={{ background: 'var(--bg-1)' }}>
                <tr className="text-text-3">
                  <th className="px-3 py-1 text-left">市场锚点时间</th>
                  <th className="px-3 py-1 text-left">组</th>
                  <th className="px-3 py-1 text-left">交易品种</th>
                  <th className="px-3 py-1 text-left">方向</th>
                  <th className="px-3 py-1 text-left">质量</th>
                  <th className="px-3 py-1 text-left">置信度</th>
                  <th className="px-3 py-1 text-left">LLM 意见</th>
                  <th className="px-3 py-1 text-left">状态</th>
                </tr>
              </thead>
              <tbody>
                {items.map((item) => {
                  const decision = item.decision;
                  return (
                    <tr
                      key={item.id}
                      className="cursor-pointer hover:bg-bg-2"
                      style={selectedId === item.id ? { background: 'rgba(245, 158, 11, 0.10)' } : undefined}
                      aria-selected={selectedId === item.id}
                      onClick={() => setSelectedId(item.id)}
                    >
                      <td className="px-3 py-1 text-text-2">{fmtTime(item.market_anchor_ts)}</td>
                      <td className="px-3 py-1 text-text-2">{groupLabel(item.watchlist_id)}</td>
                      <td className="px-3 py-1 text-text-2">{ticker(item.trade_symbol ?? '')}</td>
                      <td
                        className="px-3 py-1"
                        style={{ color: decision?.direction === 'bullish' ? 'var(--bull)' : decision?.direction === 'bearish' ? 'var(--bear)' : undefined }}
                      >
                        {decision?.direction === 'bullish' ? '↑ Bull' : decision?.direction === 'bearish' ? '↓ Bear' : decision?.direction === 'neutral' ? 'Neutral' : '-'}
                      </td>
                      <td className="px-3 py-1 text-text-2">{decision?.quality ?? '-'}</td>
                      <td className="px-3 py-1 text-text-2">{decision ? `${decision.confidence}%` : '-'}</td>
                      <td className="px-3 py-1 text-text-2">{decision ? (decision.alert ? '建议关注' : '不建议') : '-'}</td>
                      <td className={`px-3 py-1 ${item.status === 'error' ? 'text-red-400' : 'text-text-2'}`}>
                        {decisionStatusLabel(item.status)}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          {selected && (
            <aside className="min-w-[310px] flex-[2] overflow-auto border-l px-3 py-2 text-xs" style={{ borderColor: 'var(--border)' }}>
              <div className="mb-2 flex items-center justify-between gap-2">
                <div className="font-medium text-text-1">决策详情</div>
                <DrawPositionButton alertId={selected.alert_id} decisionId={selected.id} />
                <button
                  type="button"
                  className="rounded border px-2 py-1 text-text-2 disabled:cursor-not-allowed disabled:opacity-40"
                  style={{ borderColor: 'var(--border)' }}
                  disabled={!selected.alert_id || !selected.trade_symbol || !selected.validation_timeframe}
                  onClick={() => openContext(selected)}
                >
                  重新查看上下文
                </button>
              </div>
              <div className="space-y-2 text-text-2">
                <div>状态：{decisionStatusLabel(selected.status)}</div>
                <div>模型：{selected.provider}{selected.model ? ` / ${selected.model}` : ''}</div>
                {selected.error_summary && <div className="text-red-400">错误：{selected.error_summary}</div>}
                {selected.decision && (
                  <>
                    <div><span className="text-text-3">判断：</span>{selected.decision.reasoning_summary || '-'}</div>
                    <div><span className="text-text-3">证据：</span>{selected.decision.evidence_structure_ids.join('、') || '-'}</div>
                    <div>
                      <span className="text-text-3">入场区：</span>
                      {selected.decision.entry_zone
                        ? `${selected.decision.entry_zone.low}–${selected.decision.entry_zone.high}（${selected.decision.entry_zone.source}）`
                        : '-'}
                    </div>
                    <div><span className="text-text-3">失效价：</span>{selected.decision.invalidation_price ?? '-'}</div>
                    <div>
                      <span className="text-text-3">目标：</span>
                      {selected.decision.targets.map((target) => `${target.price}（${target.reason}）`).join('；') || '-'}
                    </div>
                    <div><span className="text-text-3">风险回报：</span>{selected.decision.risk_reward ?? '-'}</div>
                    <div><span className="text-text-3">等待条件：</span>{selected.decision.should_wait_for.join('；') || '-'}</div>
                    <div><span className="text-text-3">警告：</span>{selected.decision.warnings.join('；') || '-'}</div>
                  </>
                )}
              </div>
            </aside>
          )}
        </div>
      )}
    </div>
  );
}

export function BottomBar() {
  const bottomTab = useLayoutStore((s) => s.bottomTab);
  const setBottomTab = useLayoutStore((s) => s.setBottomTab);
  return (
    <div
      className="h-full flex flex-col"
      style={{
        background: 'var(--bg-1)',
        borderTop: '1px solid var(--border)',
      }}
    >
      <Tabs value={bottomTab} onValueChange={setBottomTab} className="flex-1 flex flex-col">
        <TabsList
          className="px-2 h-9"
          style={{ borderBottom: '1px solid var(--border)' }}
        >
          <TabsTrigger value="smt">SMT Inbox</TabsTrigger>
          <TabsTrigger value="candidates">Alert Inbox</TabsTrigger>
          <TabsTrigger value="alerts">Reversal Inbox</TabsTrigger>
          <TabsTrigger value="reports">Decisions（LLM 决策）</TabsTrigger>
          <TabsTrigger value="logs">Logs</TabsTrigger>
        </TabsList>
        <TabsContent value="smt" className="flex-1 min-h-0 overflow-hidden relative">
          <SmtInbox />
        </TabsContent>
        <TabsContent
          value="candidates"
          className="flex-1 min-h-0 overflow-hidden relative"
        >
          <SetupAlertInbox />
        </TabsContent>
        <TabsContent
          value="alerts"
          className="flex-1 min-h-0 overflow-hidden relative"
        >
          <ReversalInbox />
        </TabsContent>
        <TabsContent
          value="reports"
          className="flex-1 min-h-0 overflow-hidden relative"
        >
          <DecisionInbox />
        </TabsContent>
        <TabsContent
          value="logs"
          className="flex-1 min-h-0 overflow-hidden relative"
        >
          <LogsPanel />
        </TabsContent>
      </Tabs>
    </div>
  );
}
