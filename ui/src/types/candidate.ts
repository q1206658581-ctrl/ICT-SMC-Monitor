// Mirror of src-tauri/src/candidate/types.rs (M6a).

export type SetupType = 'smt';
export type SetupStatus = 'c2_confirmed' | 'validated' | 're_check' | 'expired' | 'invalidated';
export type DecisionStatus = 'new' | 'llm_pending' | 'approved' | 'rejected' | 'expired';
export type ExpiryReason = 'ttl' | 'reverse' | 'c2_break' | 'smt_invalidated';
export type DecisionMode = 'deterministic' | 'single_call' | 'multi_agent';

export type CandleRef = {
  ts: number;
  open: number;
  high: number;
  low: number;
  close: number;
};

export type Direction = 'bullish' | 'bearish';
export type ReversalConfirmKind = 'cisd' | 'mss';
export type Timeframe = string;

export type StrengthLabel = {
  symbol: string;
  label: string;
};

export type SymbolValidation = {
  event_id: string;
  symbol: string;
  kind: ReversalConfirmKind;
  direction: Direction;
  ts: number;
  price: number;
};

export type CandidateSetup = {
  id: string;
  rule_version: string;
  watchlist_id: string;
  smt_id: string;
  setup_type: SetupType;
  symbol_set: string[];
  sweeper_symbol: string;
  trade_symbols: string[];
  candidate_direction: Direction;
  context_timeframe: Timeframe;
  comparison_timeframe: Timeframe;
  validation_timeframe: Timeframe;
  context_pda_id: string | null;
  observation_window: [number, number];
  c1_candle: CandleRef;
  smt_k_candle: CandleRef;
  c2_candle: CandleRef;
  c2_case: number;
  c3_candle: CandleRef | null;
  c2_cisd_event_ids: string[];
  c3_cisd_event_ids: string[];
  validation_kind: ReversalConfirmKind | null;
  validation_symbol: string | null;
  validation_ts: number | null;
  validation_direction: Direction | null;
  validations: SymbolValidation[];
  invalidated_symbols: string[];
  symbol_invalidation_reasons: Record<string, ExpiryReason>;
  setup_status: SetupStatus;
  decision_status: DecisionStatus;
  deterministic_score: number;
  created_at: number;
  validated_at: number | null;
  expired_at: number | null;
  invalidated_at: number | null;
  expiry_reason: ExpiryReason | null;
  strength: StrengthLabel[];
  expiry_at: number | null;
  smt_rule_version: string;
};

export type DecisionLogEntry = {
  id: string;
  candidate_id: string;
  parent_id: string | null;
  provider: string;
  model: string | null;
  decision_mode: DecisionMode;
  prompt_version: string | null;
  strategy_version: string;
  context_version: string;
  request_json: string;
  raw_response: string | null;
  parsed_decision_json: string;
  parse_ok: boolean;
  error: string | null;
  created_at: number;
};
