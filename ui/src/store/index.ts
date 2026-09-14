import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import type { BarPayload } from '../types/ipc';
import type { Fvg } from '../types/structures';
import type { OverlayFilter } from '../components/chart/overlay/StructureOverlay';
import type { Watchlist } from '../types/watchlist';

export type Tf = '1m' | '5m' | '15m' | '30m' | '1h' | '4h' | '1d' | '1w';

export const TIMEFRAMES: Tf[] = [
  '1m', '5m', '15m', '30m', '1h', '4h', '1d', '1w',
];

type LayoutState = {
  leftWidth: number;
  rightWidth: number;
  rightCollapsed: boolean;
  bottomHeight: number;
  bottomCollapsed: boolean;
  bottomTab: string;
  setLeftWidth: (n: number) => void;
  setRightWidth: (n: number) => void;
  setRightCollapsed: (b: boolean) => void;
  setBottomHeight: (n: number) => void;
  setBottomCollapsed: (b: boolean) => void;
  setBottomTab: (t: string) => void;
};

export const useLayoutStore = create<LayoutState>()(
  persist(
    (set) => ({
      leftWidth: 240,
      rightWidth: 320,
      rightCollapsed: false, // M3: drawer expanded by default per kickoff §7.4
      bottomHeight: 200,
      bottomCollapsed: false,
      bottomTab: "smt",
      setLeftWidth: (n) => set({ leftWidth: n }),
      setRightWidth: (n) => set({ rightWidth: n }),
      setRightCollapsed: (b) => set({ rightCollapsed: b }),
      setBottomHeight: (n) => set({ bottomHeight: n }),
      setBottomCollapsed: (b) => set({ bottomCollapsed: b }),
      setBottomTab: (t) => set({ bottomTab: t }),
    }),
    { name: 'ict-radar-layout-v1' },
  ),
);

type ChartState = {
  symbol: string;
  tf: Tf;
  lastClose: number | null;
  lastClosedTs: number | null;
  structuresReloadNonce: number;
  barsReloadNonce: number;
  // M5 multi-symbol
  activeWatchlist: Watchlist | null;
  symbols: string[];
  singleMode: boolean;
  smtEnabled: boolean;
  smtChainEnabled: boolean;
  smtHtfPdaEnabled: boolean;
  smtSweepLineEnabled: boolean;
  highlightSmtId: string | null;
  candidateEnabled: boolean;
  candidateValidatedOnly: boolean;
  highlightCandidateId: string | null;
  alertEnabled: boolean;
  desktopNotifyEnabled: boolean;
  feishuNotifyEnabled: boolean;
  cooldownSeconds: number;
  highlightAlertId: string | null;
  highlightForceIndicators: boolean;
  setSymbol: (s: string) => void;
  setTf: (tf: Tf) => void;
  applyBar: (b: BarPayload) => void;
  requestStructuresReload: () => void;
  requestBarsReload: () => void;
  setActiveWatchlist: (wl: Watchlist) => void;
  setSingleMode: (b: boolean) => void;
  setSmtEnabled: (b: boolean) => void;
  setSmtChainEnabled: (b: boolean) => void;
  setSmtHtfPdaEnabled: (b: boolean) => void;
  setSmtSweepLineEnabled: (b: boolean) => void;
  setHighlightSmt: (id: string | null) => void;
  openSmt: (selection: { id: string; tf: Tf; symbol: string }) => void;
  setCandidateEnabled: (b: boolean) => void;
  setCandidateValidatedOnly: (b: boolean) => void;
  setHighlightCandidate: (id: string | null) => void;
  setAlertEnabled: (b: boolean) => void;
  setDesktopNotifyEnabled: (b: boolean) => void;
  setFeishuNotifyEnabled: (b: boolean) => void;
  setCooldownSeconds: (n: number) => void;
  setHighlightAlert: (id: string | null) => void;
  setHighlightForceIndicators: (b: boolean) => void;
  engineReady: boolean;
  loadStatus: 'connecting' | 'loading_bars' | 'loading_indicators' | 'ready';
  setEngineReady: (b: boolean) => void;
  setLoadStatus: (s: 'connecting' | 'loading_bars' | 'loading_indicators' | 'ready') => void;
};

export const useChartStore = create<ChartState>()(
  persist(
    (set) => ({
      symbol: 'OANDA:EURUSD',
      tf: '1m',
      lastClose: null,
      lastClosedTs: null,
      structuresReloadNonce: 0,
      barsReloadNonce: 0,
      activeWatchlist: null,
      symbols: ['OANDA:EURUSD'],
      singleMode: true,
      smtEnabled: true,
      smtChainEnabled: true,
      smtHtfPdaEnabled: true,
      smtSweepLineEnabled: true,
      highlightSmtId: null,
      candidateEnabled: true,
      candidateValidatedOnly: false,
      highlightCandidateId: null,
      alertEnabled: true,
      desktopNotifyEnabled: true,
      feishuNotifyEnabled: false,
      cooldownSeconds: 0,
      highlightAlertId: null,
      highlightForceIndicators: false,
      engineReady: false,
      loadStatus: 'connecting',
      setSymbol: (s) => set({ symbol: s }),
      setTf: (tf) => set({ tf }),
      applyBar: (b) => set({
        lastClose: b.close,
        lastClosedTs: b.closed ? b.ts : (undefined as never),
      }),
      requestStructuresReload: () => set((s) => ({ structuresReloadNonce: s.structuresReloadNonce + 1 })),
      requestBarsReload: () => set((s) => ({ barsReloadNonce: s.barsReloadNonce + 1 })),
      setActiveWatchlist: (wl) => set((state) => ({
        activeWatchlist: wl,
        symbols: wl.symbols,
        singleMode: false,
        symbol: wl.symbols[0] ?? 'OANDA:EURUSD',
        highlightSmtId: null,
        highlightCandidateId: null,
        highlightAlertId: null,
        barsReloadNonce: state.barsReloadNonce + 1,
        structuresReloadNonce: state.structuresReloadNonce + 1,
      })),
      // Layout toggles retain an explicitly chosen chart symbol. A real group
      // change resets the symbol via setActiveWatchlist above.
      setSingleMode: (b) => set({ singleMode: b }),
      setSmtEnabled: (b) => set({ smtEnabled: b }),
      setSmtChainEnabled: (b) => set({ smtChainEnabled: b }),
      setSmtHtfPdaEnabled: (b) => set({ smtHtfPdaEnabled: b }),
      setSmtSweepLineEnabled: (b) => set({ smtSweepLineEnabled: b }),
      setHighlightSmt: (id) => set({
        highlightSmtId: id,
        highlightCandidateId: null,
        highlightAlertId: null,
      }),
      // One atomic transition prevents a transient render on the previous TF
      // where the navigation box could appear without its MTF sweep line.
      openSmt: ({ id, tf, symbol }) => set((state) => ({
        symbol,
        tf,
        smtEnabled: true,
        smtChainEnabled: true,
        smtSweepLineEnabled: true,
        highlightSmtId: id,
        highlightCandidateId: null,
        highlightAlertId: null,
        structuresReloadNonce: state.structuresReloadNonce + 1,
      })),
      setCandidateEnabled: (b) => set({ candidateEnabled: b }),
      setCandidateValidatedOnly: (b) => set({
        candidateValidatedOnly: b,
        // Do not leave a previously selected non-Validated audit band on the
        // chart after its row has been removed by the display filter.
        ...(b ? { highlightCandidateId: null } : {}),
      }),
      setHighlightCandidate: (id) => set({
        highlightCandidateId: id,
        highlightSmtId: null,
        highlightAlertId: null,
      }),
      setAlertEnabled: (b) => set({ alertEnabled: b }),
      setDesktopNotifyEnabled: (b) => set({ desktopNotifyEnabled: b }),
      setFeishuNotifyEnabled: (b) => set({ feishuNotifyEnabled: b }),
      setCooldownSeconds: (n) => set({ cooldownSeconds: n }),
      setHighlightAlert: (id) => set({
        highlightAlertId: id,
        highlightSmtId: null,
        highlightCandidateId: null,
      }),
      setHighlightForceIndicators: (b) => set({ highlightForceIndicators: b }),
      setEngineReady: (b) => set({ engineReady: b }),
      setLoadStatus: (s) => set({ loadStatus: s }),
    }),
    {
      name: 'ict-radar-chart-v2',
      partialize: (s) => ({
        symbol: s.symbol,
        tf: s.tf,
        smtEnabled: s.smtEnabled,
        smtChainEnabled: s.smtChainEnabled,
        smtHtfPdaEnabled: s.smtHtfPdaEnabled,
        smtSweepLineEnabled: s.smtSweepLineEnabled,
        candidateEnabled: s.candidateEnabled,
        candidateValidatedOnly: s.candidateValidatedOnly,
        alertEnabled: s.alertEnabled,
        desktopNotifyEnabled: s.desktopNotifyEnabled,
        feishuNotifyEnabled: s.feishuNotifyEnabled,
        cooldownSeconds: s.cooldownSeconds,
      }),
    },
  ),
);

// ---- Detector toggles / params (M3) ---------------------------------------

export type DetectorState = {
  fvgEnabled: boolean;
  fvgMinSizePips: number;
  fvgStates: Set<Fvg['state']>;
  obEnabled: boolean;
  obDisplacementAtrMult: number;
  obUseBodyOnly: boolean;
  mssEnabled: boolean;
  mssFractalN: number;
  cisdEnabled: boolean;
  cisdMinLegBars: number;
  pdhPdlEnabled: boolean;
  pdhPdlMode: 'ny_local' | 'ny_1700';
  liquidityEnabled: boolean;
  liquiditySwingSweeps: boolean;
  liquidityEqhEql: boolean;
  liquidityPdhPdlSweeps: boolean;
  liquidityEqToleranceAtrMult: number;
  liquidityEqToleranceMaxPips: number;
  liquidityReversalEnabled: boolean;
  liquidityReversalMaxBars: number;
  liquidityReversalCisd: boolean;
  liquidityReversalMss: boolean;
  liquidityReversalMinScore: number;
  bosEnabled: boolean;
  breakerEnabled: boolean;
  viEnabled: boolean;
  viMinSizePips: number;
  oteEnabled: boolean;
  oteFibLow: number;
  oteFibHigh: number;
  oteConfluenceLookbackBars: number;
  premiumDiscountEnabled: boolean;
  premiumDiscountShowEqLine: boolean;
  premiumDiscountShowZones: boolean;
  premiumDiscountTolerancePips: number;
  openingGapsEnabled: boolean;
  openingGapsShowNwog: boolean;
  openingGapsShowNdog: boolean;
  openingGapsShowActive: boolean;
  openingGapsShowMitigated: boolean;
  openingGapsShowFilled: boolean;
  openingGapsMinNwogSizePips: number;
  openingGapsMinNdogSizePips: number;
  sessionsEnabled: boolean;
  sessionsShowBoxes: boolean;
  sessionsShowLabels: boolean;
  sessionsShowBackground: boolean;
  sessionsShowHighLow: boolean;
  sessionAsiaEnabled: boolean;
  sessionLondonOpenEnabled: boolean;
  sessionNewYorkOpenEnabled: boolean;
  sessionLondonCloseEnabled: boolean;
  po3Enabled: boolean;
  po3ShowMarkers: boolean;
  po3ShowStageBoxes: boolean;
  po3ShowAccumulationStage: boolean;
  po3ShowManipulationStage: boolean;
  po3ShowDistributionStage: boolean;
  po3ExecTf1m: boolean;
  po3ExecTf5m: boolean;
  po3ExecTf15m: boolean;
  po3ExecTf30m: boolean;
  po3ExecTf1h: boolean;
  po3ExecTf4h: boolean;
  po3ExecTf1d: boolean;
  po3MinAccumulationBars: number;
  po3MaxAccumulationBars: number;
  po3MaxRangeAtrMult: number;
  po3RequireLiquidityPool: boolean;
  po3MaxBarsAfterSweep: number;
  po3MinQualityScore: number;
  po3AllowCisd: boolean;
  po3AllowMss: boolean;
  setFvgEnabled: (b: boolean) => void;
  setFvgMinSizePips: (n: number) => void;
  toggleFvgState: (s: Fvg['state']) => void;
  setObEnabled: (b: boolean) => void;
  setObDisplacementAtrMult: (n: number) => void;
  setObUseBodyOnly: (b: boolean) => void;
  setMssEnabled: (b: boolean) => void;
  setMssFractalN: (n: number) => void;
  setCisdEnabled: (b: boolean) => void;
  setCisdMinLegBars: (n: number) => void;
  setPdhPdlEnabled: (b: boolean) => void;
  setPdhPdlMode: (m: 'ny_local' | 'ny_1700') => void;
  setLiquidityEnabled: (b: boolean) => void;
  setLiquiditySwingSweeps: (b: boolean) => void;
  setLiquidityEqhEql: (b: boolean) => void;
  setLiquidityPdhPdlSweeps: (b: boolean) => void;
  setLiquidityEqToleranceAtrMult: (n: number) => void;
  setLiquidityEqToleranceMaxPips: (n: number) => void;
  setLiquidityReversalEnabled: (b: boolean) => void;
  setLiquidityReversalMaxBars: (n: number) => void;
  setLiquidityReversalCisd: (b: boolean) => void;
  setLiquidityReversalMss: (b: boolean) => void;
  setLiquidityReversalMinScore: (n: number) => void;
  setBosEnabled: (b: boolean) => void;
  setBreakerEnabled: (b: boolean) => void;
  setViEnabled: (b: boolean) => void;
  setViMinSizePips: (n: number) => void;
  setOteEnabled: (b: boolean) => void;
  setOteFibLow: (n: number) => void;
  setOteFibHigh: (n: number) => void;
  setOteConfluenceLookbackBars: (n: number) => void;
  setPremiumDiscountEnabled: (b: boolean) => void;
  setPremiumDiscountShowEqLine: (b: boolean) => void;
  setPremiumDiscountShowZones: (b: boolean) => void;
  setPremiumDiscountTolerancePips: (n: number) => void;
  setOpeningGapsEnabled: (b: boolean) => void;
  setOpeningGapsShowNwog: (b: boolean) => void;
  setOpeningGapsShowNdog: (b: boolean) => void;
  setOpeningGapsShowActive: (b: boolean) => void;
  setOpeningGapsShowMitigated: (b: boolean) => void;
  setOpeningGapsShowFilled: (b: boolean) => void;
  setOpeningGapsMinNwogSizePips: (n: number) => void;
  setOpeningGapsMinNdogSizePips: (n: number) => void;
  setSessionsEnabled: (b: boolean) => void;
  setSessionsShowBoxes: (b: boolean) => void;
  setSessionsShowLabels: (b: boolean) => void;
  setSessionsShowBackground: (b: boolean) => void;
  setSessionsShowHighLow: (b: boolean) => void;
  setSessionAsiaEnabled: (b: boolean) => void;
  setSessionLondonOpenEnabled: (b: boolean) => void;
  setSessionNewYorkOpenEnabled: (b: boolean) => void;
  setSessionLondonCloseEnabled: (b: boolean) => void;
  setPo3Enabled: (b: boolean) => void;
  setPo3ShowMarkers: (b: boolean) => void;
  setPo3ShowStageBoxes: (b: boolean) => void;
  setPo3ShowAccumulationStage: (b: boolean) => void;
  setPo3ShowManipulationStage: (b: boolean) => void;
  setPo3ShowDistributionStage: (b: boolean) => void;
  setPo3ExecTf: (tf: Tf, b: boolean) => void;
  setPo3MinAccumulationBars: (n: number) => void;
  setPo3MaxAccumulationBars: (n: number) => void;
  setPo3MaxRangeAtrMult: (n: number) => void;
  setPo3RequireLiquidityPool: (b: boolean) => void;
  setPo3MaxBarsAfterSweep: (n: number) => void;
  setPo3MinQualityScore: (n: number) => void;
  setPo3AllowCisd: (b: boolean) => void;
  setPo3AllowMss: (b: boolean) => void;
};

const DEFAULT_FVG_STATES: Fvg['state'][] = ['active', 'mitigated_50', 'inverted_active'];

export const useDetectorStore = create<DetectorState>()(
  persist(
    (set, get) => ({
      fvgEnabled: true,
      fvgMinSizePips: 0,
      fvgStates: new Set<Fvg['state']>(DEFAULT_FVG_STATES),
      obEnabled: true,
      obDisplacementAtrMult: 1.5,
      obUseBodyOnly: false,
      mssEnabled: true,
      mssFractalN: 2,
      cisdEnabled: true,
      cisdMinLegBars: 2,
      pdhPdlEnabled: true,
      pdhPdlMode: 'ny_local',
      liquidityEnabled: true,
      liquiditySwingSweeps: true,
      liquidityEqhEql: true,
      liquidityPdhPdlSweeps: true,
      liquidityEqToleranceAtrMult: 0.1,
      liquidityEqToleranceMaxPips: 3,
      liquidityReversalEnabled: true,
      liquidityReversalMaxBars: 10,
      liquidityReversalCisd: true,
      liquidityReversalMss: true,
      liquidityReversalMinScore: 0,
      bosEnabled: true,
      breakerEnabled: true,
      viEnabled: true,
      viMinSizePips: 0.5,
      oteEnabled: true,
      oteFibLow: 0.62,
      oteFibHigh: 0.79,
      oteConfluenceLookbackBars: 200,
      premiumDiscountEnabled: true,
      premiumDiscountShowEqLine: true,
      premiumDiscountShowZones: false,
      premiumDiscountTolerancePips: 1.0,
      openingGapsEnabled: true,
      openingGapsShowNwog: true,
      openingGapsShowNdog: true,
      openingGapsShowActive: true,
      openingGapsShowMitigated: true,
      openingGapsShowFilled: false,
      openingGapsMinNwogSizePips: 2.0,
      openingGapsMinNdogSizePips: 0.5,
      sessionsEnabled: true,
      sessionsShowBoxes: true,
      sessionsShowLabels: true,
      sessionsShowBackground: false,
      sessionsShowHighLow: false,
      sessionAsiaEnabled: true,
      sessionLondonOpenEnabled: true,
      sessionNewYorkOpenEnabled: true,
      sessionLondonCloseEnabled: true,
      po3Enabled: true,
      po3ShowMarkers: true,
      po3ShowStageBoxes: true,
      po3ShowAccumulationStage: true,
      po3ShowManipulationStage: true,
      po3ShowDistributionStage: true,
      po3ExecTf1m: true,
      po3ExecTf5m: true,
      po3ExecTf15m: true,
      po3ExecTf30m: true,
      po3ExecTf1h: true,
      po3ExecTf4h: true,
      po3ExecTf1d: true,
      po3MinAccumulationBars: 8,
      po3MaxAccumulationBars: 30,
      po3MaxRangeAtrMult: 1.2,
      po3RequireLiquidityPool: true,
      po3MaxBarsAfterSweep: 10,
      po3MinQualityScore: 4,
      po3AllowCisd: true,
      po3AllowMss: true,
      setFvgEnabled: (b) => set({ fvgEnabled: b }),
      setFvgMinSizePips: (n) => set({ fvgMinSizePips: n }),
      toggleFvgState: (s) => {
        const next = new Set(get().fvgStates);
        if (next.has(s)) next.delete(s); else next.add(s);
        set({ fvgStates: next });
      },
      setObEnabled: (b) => set({ obEnabled: b }),
      setObDisplacementAtrMult: (n) => set({ obDisplacementAtrMult: n }),
      setObUseBodyOnly: (b) => set({ obUseBodyOnly: b }),
      setMssEnabled: (b) => set({ mssEnabled: b }),
      setMssFractalN: (n) => set({ mssFractalN: n }),
      setCisdEnabled: (b) => set({ cisdEnabled: b }),
      setCisdMinLegBars: (n) => set({ cisdMinLegBars: n }),
      setPdhPdlEnabled: (b) => set({ pdhPdlEnabled: b }),
      setPdhPdlMode: (m) => set({ pdhPdlMode: m }),
      setLiquidityEnabled: (b) => set({ liquidityEnabled: b }),
      setLiquiditySwingSweeps: (b) => set({ liquiditySwingSweeps: b }),
      setLiquidityEqhEql: (b) => set({ liquidityEqhEql: b }),
      setLiquidityPdhPdlSweeps: (b) => set({ liquidityPdhPdlSweeps: b }),
      setLiquidityEqToleranceAtrMult: (n) => set({ liquidityEqToleranceAtrMult: n }),
      setLiquidityEqToleranceMaxPips: (n) => set({ liquidityEqToleranceMaxPips: n }),
      setLiquidityReversalEnabled: (b) => set({ liquidityReversalEnabled: b }),
      setLiquidityReversalMaxBars: (n) => set({ liquidityReversalMaxBars: n }),
      setLiquidityReversalCisd: (b) => set({ liquidityReversalCisd: b }),
      setLiquidityReversalMss: (b) => set({ liquidityReversalMss: b }),
      setLiquidityReversalMinScore: (n) => set({ liquidityReversalMinScore: n }),
      setBosEnabled: (b) => set({ bosEnabled: b }),
      setBreakerEnabled: (b) => set({ breakerEnabled: b }),
      setViEnabled: (b) => set({ viEnabled: b }),
      setViMinSizePips: (n) => set({ viMinSizePips: n }),
      setOteEnabled: (b) => set({ oteEnabled: b }),
      setOteFibLow: (n) => set({ oteFibLow: n }),
      setOteFibHigh: (n) => set({ oteFibHigh: n }),
      setOteConfluenceLookbackBars: (n) => set({ oteConfluenceLookbackBars: n }),
      setPremiumDiscountEnabled: (b) => set({ premiumDiscountEnabled: b }),
      setPremiumDiscountShowEqLine: (b) => set({ premiumDiscountShowEqLine: b }),
      setPremiumDiscountShowZones: (b) => set({ premiumDiscountShowZones: b }),
      setPremiumDiscountTolerancePips: (n) => set({ premiumDiscountTolerancePips: n }),
      setOpeningGapsEnabled: (b) => set({ openingGapsEnabled: b }),
      setOpeningGapsShowNwog: (b) => set({ openingGapsShowNwog: b }),
      setOpeningGapsShowNdog: (b) => set({ openingGapsShowNdog: b }),
      setOpeningGapsShowActive: (b) => set({ openingGapsShowActive: b }),
      setOpeningGapsShowMitigated: (b) => set({ openingGapsShowMitigated: b }),
      setOpeningGapsShowFilled: (b) => set({ openingGapsShowFilled: b }),
      setOpeningGapsMinNwogSizePips: (n) => set({ openingGapsMinNwogSizePips: n }),
      setOpeningGapsMinNdogSizePips: (n) => set({ openingGapsMinNdogSizePips: n }),
      setSessionsEnabled: (b) => set({ sessionsEnabled: b }),
      setSessionsShowBoxes: (b) => set({ sessionsShowBoxes: b }),
      setSessionsShowLabels: (b) => set({ sessionsShowLabels: b }),
      setSessionsShowBackground: (b) => set({ sessionsShowBackground: b }),
      setSessionsShowHighLow: (b) => set({ sessionsShowHighLow: b }),
      setSessionAsiaEnabled: (b) => set({ sessionAsiaEnabled: b }),
      setSessionLondonOpenEnabled: (b) => set({ sessionLondonOpenEnabled: b }),
      setSessionNewYorkOpenEnabled: (b) => set({ sessionNewYorkOpenEnabled: b }),
      setSessionLondonCloseEnabled: (b) => set({ sessionLondonCloseEnabled: b }),
      setPo3Enabled: (b) => set({ po3Enabled: b }),
      setPo3ShowMarkers: (b) => set({ po3ShowMarkers: b }),
      setPo3ShowStageBoxes: (b) => set({ po3ShowStageBoxes: b }),
      setPo3ShowAccumulationStage: (b) => set({ po3ShowAccumulationStage: b }),
      setPo3ShowManipulationStage: (b) => set({ po3ShowManipulationStage: b }),
      setPo3ShowDistributionStage: (b) => set({ po3ShowDistributionStage: b }),
      setPo3ExecTf: (tf, b) => set({ [po3ExecTfKey(tf)]: b } as Partial<DetectorState>),
      setPo3MinAccumulationBars: (n) => set({ po3MinAccumulationBars: n }),
      setPo3MaxAccumulationBars: (n) => set({ po3MaxAccumulationBars: n }),
      setPo3MaxRangeAtrMult: (n) => set({ po3MaxRangeAtrMult: n }),
      setPo3RequireLiquidityPool: (b) => set({ po3RequireLiquidityPool: b }),
      setPo3MaxBarsAfterSweep: (n) => set({ po3MaxBarsAfterSweep: n }),
      setPo3MinQualityScore: (n) => set({ po3MinQualityScore: n }),
      setPo3AllowCisd: (b) => set({ po3AllowCisd: b }),
      setPo3AllowMss: (b) => set({ po3AllowMss: b }),
    }),
    {
      name: 'ict-radar-detector-v1',
      partialize: (s) => ({
        fvgEnabled: s.fvgEnabled,
        fvgMinSizePips: s.fvgMinSizePips,
        fvgStates: Array.from(s.fvgStates),
        obEnabled: s.obEnabled,
        obDisplacementAtrMult: s.obDisplacementAtrMult,
        obUseBodyOnly: s.obUseBodyOnly,
        mssEnabled: s.mssEnabled,
        mssFractalN: s.mssFractalN,
        cisdEnabled: s.cisdEnabled,
        cisdMinLegBars: s.cisdMinLegBars,
        pdhPdlEnabled: s.pdhPdlEnabled,
        pdhPdlMode: s.pdhPdlMode,
        liquidityEnabled: s.liquidityEnabled,
        liquiditySwingSweeps: s.liquiditySwingSweeps,
        liquidityEqhEql: s.liquidityEqhEql,
        liquidityPdhPdlSweeps: s.liquidityPdhPdlSweeps,
        liquidityEqToleranceAtrMult: s.liquidityEqToleranceAtrMult,
        liquidityEqToleranceMaxPips: s.liquidityEqToleranceMaxPips,
        liquidityReversalEnabled: s.liquidityReversalEnabled,
        liquidityReversalMaxBars: s.liquidityReversalMaxBars,
        liquidityReversalCisd: s.liquidityReversalCisd,
        liquidityReversalMss: s.liquidityReversalMss,
        liquidityReversalMinScore: s.liquidityReversalMinScore,
        bosEnabled: s.bosEnabled,
        breakerEnabled: s.breakerEnabled,
        viEnabled: s.viEnabled,
        viMinSizePips: s.viMinSizePips,
        oteEnabled: s.oteEnabled,
        oteFibLow: s.oteFibLow,
        oteFibHigh: s.oteFibHigh,
        oteConfluenceLookbackBars: s.oteConfluenceLookbackBars,
        premiumDiscountEnabled: s.premiumDiscountEnabled,
        premiumDiscountShowEqLine: s.premiumDiscountShowEqLine,
        premiumDiscountShowZones: s.premiumDiscountShowZones,
        premiumDiscountTolerancePips: s.premiumDiscountTolerancePips,
        openingGapsEnabled: s.openingGapsEnabled,
        openingGapsShowNwog: s.openingGapsShowNwog,
        openingGapsShowNdog: s.openingGapsShowNdog,
        openingGapsShowActive: s.openingGapsShowActive,
        openingGapsShowMitigated: s.openingGapsShowMitigated,
        openingGapsShowFilled: s.openingGapsShowFilled,
        openingGapsMinNwogSizePips: s.openingGapsMinNwogSizePips,
        openingGapsMinNdogSizePips: s.openingGapsMinNdogSizePips,
        sessionsEnabled: s.sessionsEnabled,
        sessionsShowBoxes: s.sessionsShowBoxes,
        sessionsShowLabels: s.sessionsShowLabels,
        sessionsShowBackground: s.sessionsShowBackground,
        sessionsShowHighLow: s.sessionsShowHighLow,
        sessionAsiaEnabled: s.sessionAsiaEnabled,
        sessionLondonOpenEnabled: s.sessionLondonOpenEnabled,
        sessionNewYorkOpenEnabled: s.sessionNewYorkOpenEnabled,
        sessionLondonCloseEnabled: s.sessionLondonCloseEnabled,
        po3Enabled: s.po3Enabled,
        po3ShowMarkers: s.po3ShowMarkers,
        po3ShowStageBoxes: s.po3ShowStageBoxes,
        po3ShowAccumulationStage: s.po3ShowAccumulationStage,
        po3ShowManipulationStage: s.po3ShowManipulationStage,
        po3ShowDistributionStage: s.po3ShowDistributionStage,
        po3ExecTf1m: s.po3ExecTf1m,
        po3ExecTf5m: s.po3ExecTf5m,
        po3ExecTf15m: s.po3ExecTf15m,
        po3ExecTf30m: s.po3ExecTf30m,
        po3ExecTf1h: s.po3ExecTf1h,
        po3ExecTf4h: s.po3ExecTf4h,
        po3ExecTf1d: s.po3ExecTf1d,
        po3MinAccumulationBars: s.po3MinAccumulationBars,
        po3MaxAccumulationBars: s.po3MaxAccumulationBars,
        po3MaxRangeAtrMult: s.po3MaxRangeAtrMult,
        po3RequireLiquidityPool: s.po3RequireLiquidityPool,
        po3MaxBarsAfterSweep: s.po3MaxBarsAfterSweep,
        po3MinQualityScore: s.po3MinQualityScore,
        po3AllowCisd: s.po3AllowCisd,
        po3AllowMss: s.po3AllowMss,
      }) as unknown as DetectorState,
      // zustand persist hydrates Sets as plain arrays — convert back here.
      onRehydrateStorage: () => (state) => {
        if (!state) return;
        const hydratedFvgStates = state.fvgStates as Set<Fvg['state']> | Fvg['state'][];
        if (Array.isArray(hydratedFvgStates)) {
          state.fvgStates = new Set(hydratedFvgStates);
        }
      },
    },
  ),
);

/** Selector: derive the OverlayFilter from the detector store. */
export function useDetectorFilters(): OverlayFilter {
  const forceIndicators = useChartStore((s) => s.highlightForceIndicators);
  const fvgEnabled = useDetectorStore((s) => s.fvgEnabled);
  const fvgStates = useDetectorStore((s) => s.fvgStates);
  const obEnabled = useDetectorStore((s) => s.obEnabled);
  const mssEnabled = useDetectorStore((s) => s.mssEnabled);
  const cisdEnabled = useDetectorStore((s) => s.cisdEnabled);
  const pdhPdlEnabled = useDetectorStore((s) => s.pdhPdlEnabled);
  const liquidityEnabled = useDetectorStore((s) => s.liquidityEnabled);
  const liquiditySwingSweeps = useDetectorStore((s) => s.liquiditySwingSweeps);
  const liquidityEqhEql = useDetectorStore((s) => s.liquidityEqhEql);
  const liquidityPdhPdlSweeps = useDetectorStore((s) => s.liquidityPdhPdlSweeps);
  const liquidityReversalEnabled = useDetectorStore((s) => s.liquidityReversalEnabled);
  const liquidityReversalCisd = useDetectorStore((s) => s.liquidityReversalCisd);
  const liquidityReversalMss = useDetectorStore((s) => s.liquidityReversalMss);
  const bosEnabled = useDetectorStore((s) => s.bosEnabled);
  const breakerEnabled = useDetectorStore((s) => s.breakerEnabled);
  const viEnabled = useDetectorStore((s) => s.viEnabled);
  const oteEnabled = useDetectorStore((s) => s.oteEnabled);
  const premiumDiscountEnabled = useDetectorStore((s) => s.premiumDiscountEnabled);
  const premiumDiscountShowEqLine = useDetectorStore((s) => s.premiumDiscountShowEqLine);
  const premiumDiscountShowZones = useDetectorStore((s) => s.premiumDiscountShowZones);
  const openingGapsEnabled = useDetectorStore((s) => s.openingGapsEnabled);
  const openingGapsShowNwog = useDetectorStore((s) => s.openingGapsShowNwog);
  const openingGapsShowNdog = useDetectorStore((s) => s.openingGapsShowNdog);
  const openingGapsShowActive = useDetectorStore((s) => s.openingGapsShowActive);
  const openingGapsShowMitigated = useDetectorStore((s) => s.openingGapsShowMitigated);
  const openingGapsShowFilled = useDetectorStore((s) => s.openingGapsShowFilled);
  const sessionsEnabled = useDetectorStore((s) => s.sessionsEnabled);
  const sessionsShowBoxes = useDetectorStore((s) => s.sessionsShowBoxes);
  const sessionsShowLabels = useDetectorStore((s) => s.sessionsShowLabels);
  const sessionsShowBackground = useDetectorStore((s) => s.sessionsShowBackground);
  const sessionsShowHighLow = useDetectorStore((s) => s.sessionsShowHighLow);
  const sessionAsiaEnabled = useDetectorStore((s) => s.sessionAsiaEnabled);
  const sessionLondonOpenEnabled = useDetectorStore((s) => s.sessionLondonOpenEnabled);
  const sessionNewYorkOpenEnabled = useDetectorStore((s) => s.sessionNewYorkOpenEnabled);
  const sessionLondonCloseEnabled = useDetectorStore((s) => s.sessionLondonCloseEnabled);
  const po3Enabled = useDetectorStore((s) => s.po3Enabled);
  const po3ShowMarkers = useDetectorStore((s) => s.po3ShowMarkers);
  const po3ShowStageBoxes = useDetectorStore((s) => s.po3ShowStageBoxes);
  const po3ShowAccumulationStage = useDetectorStore((s) => s.po3ShowAccumulationStage);
  const po3ShowManipulationStage = useDetectorStore((s) => s.po3ShowManipulationStage);
  const po3ShowDistributionStage = useDetectorStore((s) => s.po3ShowDistributionStage);
  return {
    showFvg: fvgEnabled,
    fvgStates,
    showOb: obEnabled,
    showMss: mssEnabled || forceIndicators,
    showCisd: cisdEnabled || forceIndicators,
    forceIndicators,
    showPdhPdl: pdhPdlEnabled,
    showLiquidity: liquidityEnabled,
    showSwingSweeps: liquiditySwingSweeps,
    showEqhEql: liquidityEqhEql,
    showPdhPdlSweeps: liquidityPdhPdlSweeps,
    showLiquidityReversal: liquidityReversalEnabled,
    showLiquidityReversalCisd: liquidityReversalCisd,
    showLiquidityReversalMss: liquidityReversalMss,
    showBos: bosEnabled,
    showBreaker: breakerEnabled,
    showVi: viEnabled,
    showOte: oteEnabled,
    showPremiumDiscount: premiumDiscountEnabled,
    showPremiumDiscountEqLine: premiumDiscountShowEqLine,
    showPremiumDiscountZones: premiumDiscountShowZones,
    showOpeningGaps: openingGapsEnabled,
    showNwog: openingGapsShowNwog,
    showNdog: openingGapsShowNdog,
    showOpeningGapActive: openingGapsShowActive,
    showOpeningGapMitigated: openingGapsShowMitigated,
    showOpeningGapFilled: openingGapsShowFilled,
    showSessions: sessionsEnabled,
    showSessionBoxes: sessionsShowBoxes,
    showSessionLabels: sessionsShowLabels,
    showSessionBackground: sessionsShowBackground,
    showSessionHighLow: sessionsShowHighLow,
    showSessionAsia: sessionAsiaEnabled,
    showSessionLondonOpen: sessionLondonOpenEnabled,
    showSessionNewYorkOpen: sessionNewYorkOpenEnabled,
    showSessionLondonClose: sessionLondonCloseEnabled,
    showPo3: po3Enabled,
    showPo3Markers: po3ShowMarkers,
    showPo3StageBoxes: po3ShowStageBoxes,
    showPo3AccumulationStage: po3ShowAccumulationStage,
    showPo3ManipulationStage: po3ShowManipulationStage,
    showPo3DistributionStage: po3ShowDistributionStage,
  };
}

function po3ExecTfKey(tf: Tf): keyof DetectorState {
  switch (tf) {
    case '1m': return 'po3ExecTf1m';
    case '5m': return 'po3ExecTf5m';
    case '15m': return 'po3ExecTf15m';
    case '30m': return 'po3ExecTf30m';
    case '1h': return 'po3ExecTf1h';
    case '4h': return 'po3ExecTf4h';
    case '1d': return 'po3ExecTf1d';
    case '1w': return 'po3ExecTf1d';
  }
}
