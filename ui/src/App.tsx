import { useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useChartStore, useDetectorStore } from './store';
import {
  Panel,
  PanelGroup,
  PanelResizeHandle,
} from 'react-resizable-panels';
import { Toaster, toast } from 'sonner';
import { listen } from '@tauri-apps/api/event';
import { TopBar } from './components/layout/TopBar';
import { LeftSide } from './components/layout/LeftSide';
import { ChartToolbar } from './components/layout/ChartToolbar';
import { RightDrawer } from './components/layout/RightDrawer';
import { BottomBar } from './components/layout/BottomBar';
import { MultiPaneContainer } from './components/chart/MultiPaneContainer';
import { useLayoutStore } from './store';
import type { AppStatusPayload } from './types/ipc';
import type { AlertRecord } from './types/alert';
import type { Watchlist } from './types/watchlist';
import { TooltipProvider } from './components/ui/tooltip';
import '@fontsource/inter/400.css';
import '@fontsource/inter/500.css';
import '@fontsource/inter/600.css';
import '@fontsource/jetbrains-mono/400.css';
import '@fontsource/jetbrains-mono/500.css';

function groupLabel(id: string) {
  switch (id) {
    case 'eu-gu-dxy': return 'EU/GU';
    case 'aud-nzd-dxy': return 'AUD/NZD';
    case 'chf-cad-dxy': return 'CHF/CAD';
    default: return id || 'Unknown';
  }
}

export function App() {
  const layout = useLayoutStore();

  useEffect(() => {
    const p = listen<AppStatusPayload>('app:status', (e) => {
      const { kind, message } = e.payload;
      if (kind === 'error' || kind === 'fatal') {
        useChartStore.getState().setLoadStatus('connecting');
        toast.error(message);
      } else if (kind === 'connected') {
        // Bar/indicator loaders own the remaining ready transition.
        if (!useChartStore.getState().engineReady) {
          useChartStore.getState().setLoadStatus('loading_bars');
        }
      } else {
        toast(message);
      }
    });
    return () => {
      p.then((u) => u());
    };
  }, []);

  // A per-symbol C2 confirmation is actionable and remains visible until the
  // user dismisses it. Later reversal confirmations are audit-only facts.
  useEffect(() => {
    const p = listen<AlertRecord>('ict:alert:fired', (event) => {
      const alert = event.payload;
      const symbols = alert.trade_symbols.map((symbol) => symbol.replace(/.*:/, '')).join(' / ');
      const group = groupLabel(alert.watchlist_id);
      const arrow = alert.validation_direction === 'bullish' ? '↑' : alert.validation_direction === 'bearish' ? '↓' : '';
      toast(`[${group}] ${symbols} C2 已确认 ${arrow}`, {
        id: alert.id,
        description: `Case ${alert.c2_case || '-'} · 请打开图表确认进场时机 · 评分 ${alert.deterministic_score.toFixed(2)}`,
        duration: Infinity,
        closeButton: true,
      });
    });
    return () => { p.then((unlisten) => unlisten()); };
  }, []);

  // Re-fetch structures once the backend finishes its cold-start seed so the
  // chart reflects the seeded detectors without a manual toggle.
  useEffect(() => {
    const p = listen('seed_complete', () => {
      useChartStore.getState().setLoadStatus('loading_bars');
      useChartStore.getState().requestBarsReload();
      useChartStore.getState().setEngineReady(true);
    });
    return () => {
      p.then((u) => u());
    };
  }, []);

  // After the TV Historical backfill the backend replays po3 over the full
  // bucket history (the cold-start seed only had ~500 bars and skipped po3).
  // Re-fetch so the chart picks up the complete PO3 set automatically.
  useEffect(() => {
    const p = listen('po3_replay_done', () => {
      useChartStore.getState().setLoadStatus('loading_bars');
      useChartStore.getState().requestBarsReload();
    });
    return () => {
      p.then((u) => u());
    };
  }, []);

  // M5: load the backend's already-active watchlist. The backend restores
  // and seeds it during startup; invoking set_active_watchlist again here
  // used to clear/replay SMT a second time and start subscriptions while the
  // cold-start seed still held the detector engine.
  useEffect(() => {
    async function loadWatchlists() {
      try {
        const lists = await invoke<Watchlist[]>('list_watchlists');
        if (lists.length > 0) {
          const active = await invoke<Watchlist | null>('get_active_watchlist');
          const target = active ?? lists[0];
          useChartStore.getState().setActiveWatchlist(target);
        }
      } catch (e) {
        console.warn('watchlist load failed', e);
      }
    }
    void loadWatchlists();
  }, []);

  // Push persisted detector params back to the backend on every cold start.
  // PO3 uses the config-only variant (no replay) so the detector adopts the
  // user's thresholds before the post-backfill `replay_po3_full_history`
  // runs; the `po3_replay_done` listener above re-fetches the results.
  // pdh_pdl still replays here (single level, cheap).
  useEffect(() => {
    const state = useDetectorStore.getState();
    const po3Updates: Array<{ key: string; value: unknown }> = [
      { key: 'enabled', value: state.po3Enabled },
      { key: 'enabled_1m', value: state.po3ExecTf1m },
      { key: 'enabled_5m', value: state.po3ExecTf5m },
      { key: 'enabled_15m', value: state.po3ExecTf15m },
      { key: 'enabled_30m', value: state.po3ExecTf30m },
      { key: 'enabled_1h', value: state.po3ExecTf1h },
      { key: 'enabled_4h', value: state.po3ExecTf4h },
      { key: 'enabled_1d', value: state.po3ExecTf1d },
      { key: 'min_accumulation_bars', value: state.po3MinAccumulationBars },
      { key: 'max_accumulation_bars', value: state.po3MaxAccumulationBars },
      { key: 'max_range_atr_mult', value: state.po3MaxRangeAtrMult },
      { key: 'require_liquidity_pool', value: state.po3RequireLiquidityPool },
      { key: 'max_bars_after_sweep', value: state.po3MaxBarsAfterSweep },
      { key: 'min_quality_score', value: state.po3MinQualityScore },
      { key: 'allow_cisd', value: state.po3AllowCisd },
      { key: 'allow_mss', value: state.po3AllowMss },
    ];

    async function syncDetectorParams() {
      await invoke('set_detector_param', {
        name: 'pdh_pdl',
        key: 'daily_boundary',
        value: state.pdhPdlMode,
      });
      await invoke('set_detector_params_config_only', { name: 'po3', updates: po3Updates });
      useChartStore.getState().requestStructuresReload();
    }

    void syncDetectorParams().catch((e) => console.warn('initial detector param sync failed', e));
  }, []);

  return (
    <TooltipProvider delayDuration={200}>
      <div className="h-screen w-screen flex flex-col"
        style={{ background: 'var(--bg-0)', color: 'var(--text-1)' }}
      >
        <TopBar />
        <div className="flex-1 min-h-0">
          <PanelGroup direction="vertical" autoSaveId="ict-radar-vert">
            <Panel defaultSize={70} minSize={40}>
              <PanelGroup direction="horizontal" autoSaveId="ict-radar-horz">
                <Panel
                  defaultSize={Math.round((layout.leftWidth / 1440) * 100)}
                  minSize={12}
                  maxSize={30}
                  onResize={(s: number) => layout.setLeftWidth(Math.round((s / 100) * 1440))}
                >
                  <LeftSide />
                </Panel>
                <PanelResizeHandle className="w-px bg-[var(--border)] hover:bg-[var(--border-strong)] transition-colors" />
                <Panel minSize={30}>
                  <div className="h-full flex flex-col">
                    <ChartToolbar />
                    <div className="flex-1 min-h-0">
                      <MultiPaneContainer />
                    </div>
                  </div>
                </Panel>
                {!layout.rightCollapsed && (
                  <>
                    <PanelResizeHandle className="w-px bg-[var(--border)] hover:bg-[var(--border-strong)] transition-colors" />
                    <Panel
                      defaultSize={Math.round((layout.rightWidth / 1440) * 100)}
                      minSize={15}
                      maxSize={35}
                      onResize={(s: number) => layout.setRightWidth(Math.round((s / 100) * 1440))}
                    >
                      <RightDrawer />
                    </Panel>
                  </>
                )}
              </PanelGroup>
            </Panel>
            {!layout.bottomCollapsed && (
              <>
                <PanelResizeHandle className="h-px bg-[var(--border)] hover:bg-[var(--border-strong)] transition-colors" />
                <Panel
                  defaultSize={25}
                  minSize={10}
                  maxSize={50}
                  onResize={(s: number) => layout.setBottomHeight(Math.round((s / 100) * 900))}
                >
                  <BottomBar />
                </Panel>
              </>
            )}
          </PanelGroup>
        </div>
        <Toaster
          theme="dark"
          position="top-right"
          toastOptions={{
            style: {
              background: 'var(--bg-2)',
              color: 'var(--text-1)',
              border: '1px solid var(--border)',
            },
          }}
        />
      </div>
    </TooltipProvider>
  );
}

export default App;
