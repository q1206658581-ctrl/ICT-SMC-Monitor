// Wire-shapes the Tauri backend ships in `ict:structure:*` events.
// Mirror of `src-tauri/src/detector/types.rs`.

export type Direction = 'bullish' | 'bearish';
export type FvgState =
  | 'active'
  | 'mitigated_50'
  | 'filled'
  | 'inverted_active'
  | 'inverted_mitigated';
export type ObState = 'active' | 'tested' | 'mitigated';
export type ZoneState = 'active' | 'tested' | 'mitigated' | 'filled' | 'invalidated';
export type GapState = 'active' | 'mitigated_50' | 'filled';
export type PdSide = 'premium' | 'discount' | 'equilibrium';
export type OpeningGapKind = 'nwog' | 'ndog';
export type GapDirection = 'up' | 'down';
export type SessionKind = 'asia' | 'london_open' | 'new_york_open' | 'london_close';

export type Fvg = {
  kind: 'fvg';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  ts_open: number;
  ts_confirm: number;
  price_low: number;
  price_high: number;
  state: FvgState;
  consumed_exit_ts?: number | null;
};

export type OrderBlock = {
  kind: 'order_block';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  ts_open: number;
  ts_confirm: number;
  price_low: number;
  price_high: number;
  state: ObState;
};

export type BreakerBlock = {
  kind: 'breaker_block';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  source_ob_id: string;
  ts_open: number;
  ts_confirm: number;
  price_low: number;
  price_high: number;
  state: ZoneState;
};

export type Mss = {
  kind: 'mss';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  break_ts: number;
  break_price: number;
  swing_ts: number;
  swing_price: number;
};

export type Bos = {
  kind: 'bos';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  break_ts: number;
  break_price: number;
  swing_ts: number;
  swing_price: number;
};

export type Cisd = {
  kind: 'cisd';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  leg_origin_ts: number;
  leg_origin_price: number;
  break_ts: number;
  break_price: number;
};

export type LevelMarker = {
  kind: 'pdh' | 'pdl';
  id: string;
  symbol: string;
  tf: string;
  price: number;
  label: string;
  valid_from_ts: number;
  valid_until_ts: number;
  source_ts?: number;
};

export type LiquidityPoolKind = 'swing_high' | 'swing_low' | 'equal_highs' | 'equal_lows' | 'pdh' | 'pdl';
export type LiquiditySide = 'buy_side' | 'sell_side';
export type ReversalConfirmKind = 'cisd' | 'mss';
export type Po3ContextKind = 'session_asia' | 'generic_range' | 'htf_fvg' | 'htf_order_block' | 'htf_breaker' | 'htf_ote' | 'htf_premium_discount' | 'mixed_htf_context';
export type Po3State = 'accumulation_candidate' | 'manipulation_swept' | 'early_reversal' | 'reversal_confirmed' | 'distribution_confirmed';
export type Po3Stage = 'accumulation' | 'manipulation' | 'distribution';

export type Po3StageBox = {
  stage: Po3Stage;
  ts_start: number;
  ts_end: number;
  price_low: number;
  price_high: number;
  label?: string;
};

export type LiquiditySweep = {
  kind: 'liquidity_sweep';
  id: string;
  symbol: string;
  tf: string;
  side: LiquiditySide;
  pool_kind: LiquidityPoolKind;
  sweep_ts: number;
  sweep_price: number;
  level_ts: number;
  level_price: number;
  close_price: number;
};

export type EqualHighsLows = {
  kind: 'equal_highs_lows';
  id: string;
  symbol: string;
  tf: string;
  side: LiquiditySide;
  ts_start: number;
  ts_end: number;
  price: number;
  tolerance_price: number;
  swept: boolean;
};

export type LiquidityReversal = {
  kind: 'liquidity_reversal';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  sweep_id: string;
  sweep_pool_kind?: LiquiditySweep['pool_kind'];
  sweep_side?: LiquiditySweep['side'];
  confirm_id: string;
  confirm_kind: ReversalConfirmKind;
  sweep_ts: number;
  sweep_level_ts?: number;
  confirm_ts: number;
  level_price: number;
  confirm_price?: number;
  score: number;
};

export type VolumeImbalance = {
  kind: 'volume_imbalance';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  ts_open: number;
  ts_confirm: number;
  price_low: number;
  price_high: number;
  state: GapState;
};

export type OteZone = {
  kind: 'ote';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  leg_start_ts: number;
  leg_end_ts: number;
  leg_low: number;
  leg_high: number;
  price_low: number;
  price_high: number;
  fib_low: number;
  fib_high: number;
  confluent_structure_ids: string[];
};

export type PremiumDiscount = {
  kind: 'premium_discount';
  id: string;
  symbol: string;
  tf: string;
  range_start_ts: number;
  range_end_ts: number;
  high: number;
  low: number;
  equilibrium: number;
  current_side: PdSide;
  current_price: number;
};

export type GapZone = {
  kind: 'nwog' | 'ndog';
  id: string;
  symbol: string;
  tf: string;
  gap_kind: OpeningGapKind;
  direction: GapDirection;
  ts_start: number;
  ts_end: number;
  prev_close_ts: number;
  new_open_ts: number;
  prev_close: number;
  new_open: number;
  price_low: number;
  price_high: number;
  state: GapState;
};

export type KillZoneWindow = {
  kind: 'kill_zone_window';
  id: string;
  symbol: string;
  tf: string;
  session: SessionKind;
  label: string;
  ts_start: number;
  ts_end: number;
};

export type SessionRange = {
  kind: 'session_range';
  id: string;
  symbol: string;
  tf: string;
  session: SessionKind;
  label: string;
  ts_start: number;
  ts_end: number;
  high: number;
  low: number;
  high_ts: number;
  low_ts: number;
  finalized: boolean;
};

export type PowerOf3 = {
  kind: 'power_of_3';
  id: string;
  symbol: string;
  tf: string;
  direction: Direction;
  state: Po3State;
  context_kind: Po3ContextKind;
  context_structure_ids: string[];
  context_timeframes: string[];
  accumulation_start_ts: number;
  accumulation_end_ts: number;
  accumulation_high: number;
  accumulation_low: number;
  sweep_ts: number;
  sweep_price: number;
  confirm_ts: number;
  confirm_id: string;
  confirm_kind: ReversalConfirmKind;
  entry_ts?: number;
  entry_price?: number;
  entry_tf?: string;
  entry_kind?: ReversalConfirmKind;
  entry_id?: string;
  bos_id?: string;
  quality_score: number;
  stage_boxes: Po3StageBox[];
};

export type KillZoneKind =
  | 'asia'
  | 'london_open'
  | 'new_york_open'
  | 'london_close'
  | 'silver_bullet_asia'
  | 'silver_bullet_london'
  | 'silver_bullet_new_york';

export type KillZoneSpan = {
  kind: 'kill_zone';
  id: string;
  symbol: string;
  tf: string;
  kz_kind: KillZoneKind;
  label: string;
  ts_start: number;
  ts_end: number;
};

export type CandleRef = {
  ts: number;
  open: number;
  high: number;
  low: number;
  close: number;
};

export type LiquidityRefStatus = 'swept' | 'not_swept' | 'equal_high_low' | 'unknown';

export type LiquidityRef = {
  symbol: string;
  ref_price: number;
  ref_ts: number;
  side: 'buy_side' | 'sell_side';
  status: LiquidityRefStatus;
  tf: string;
  mtf_ref_candle?: CandleRef | null;
  mtf_sweep_candle?: CandleRef | null;
};

export type ReferenceScope = 'distant_left_side' | 'local_near_pda';

export type SmtReferenceEvidence = {
  ref_price: number;
  ref_ts: number;
  side: 'buy_side' | 'sell_side';
  tf: string;
  scope: ReferenceScope;
};

export type PdaRef = {
  kind: string; // "ob" | "fvg"
  id: string;
  tf: string;
  direction: Direction; // bullish=BISI, bearish=SIBI
  price_low: number;
  price_high: number;
  ts_open: number;
  ts_confirm: number;
  exit_ts?: number | null;
  ts_filled?: number | null;
};

export type StrengthLabel = {
  symbol: string;
  label: string; // "strong" | "weak"
};

export type SmtDetectionState =
  | 'smt_k_detected'
  | 'c2_confirmed'
  | 'c3_entry'
  | 'invalidated';

export type SmtInvalidationReason =
  | 'sweeper_c2_failed'
  | 'sweeper_c3_failed'
  | 'all_counters_swept'
  | 'canonical_reference_changed'
  | 'canonical_sweep_invalid'
  | 'reference_already_taken'
  | 'canonical_chain_changed'
  | 'pda_consumed_by_other_smt'
  | 'htf_formation_cancelled';

export type SymbolChain = {
  symbol: string;
  c1_candle: CandleRef;
  smt_k_candle: CandleRef;
  c2_candle: CandleRef | null;
  c2_case: number | null;
  c3_candle: CandleRef | null;
  detection_state: SmtDetectionState;
};

export type SmtDivergence = {
  kind: 'smt_divergence';
  id: string;
  watchlist_id: string;
  rule_version: string;
  symbol_set: string[];
  relationship: 'positive' | 'negative';
  context_timeframe: string;
  comparison_timeframe: string;
  observation_window: [number, number];
  htf_confirmed: boolean;
  reference_scope: ReferenceScope;
  liquidity_refs: LiquidityRef[];
  confluence_refs?: SmtReferenceEvidence[];
  candidate_direction: Direction;
  sweeper_symbol: string;
  trade_symbols: string[];
  strength: StrengthLabel[];
  chains: SymbolChain[];
  /** Empty/absent on valid SMTs and legacy invalidated rows. */
  invalidation_reasons?: SmtInvalidationReason[];
  /** Market transition timestamp; absent on legacy/non-terminal rows. */
  invalidation_ts?: number;
  htf_pda_ref: PdaRef | null;
  // DXY 在完整 HTF 参照区间内做出极值的那根 MTF K。
  // 其他品种使用 liquidity_refs[*].mtf_ref_candle，时间可不同。
  mtf_ref_candle: CandleRef | null;
};

export type IctStructure =
  | Fvg
  | OrderBlock
  | BreakerBlock
  | Mss
  | Bos
  | Cisd
  | LevelMarker
  | KillZoneSpan
  | LiquiditySweep
  | EqualHighsLows
  | LiquidityReversal
  | VolumeImbalance
  | OteZone
  | PremiumDiscount
  | GapZone
  | KillZoneWindow
  | SessionRange
  | PowerOf3
  | SmtDivergence;

export type StructureNew = { op: 'new' } & IctStructure;
export type StructureUpdate = { op: 'update' } & IctStructure;
export type StructureInvalidated = { op: 'invalidated'; id: string; kind: string };
export type StructureEvent = StructureNew | StructureUpdate | StructureInvalidated;
