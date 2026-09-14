// Mirror of src-tauri/src/alert/types.rs (M6b).

import type { ExpiryReason } from './candidate';

export type AlertTrigger = 'C2Confirmed' | 'Validated';
export type ChannelKind = 'Inbox' | 'DesktopNotify' | 'FeishuNotify';

export type AlertRecord = {
  id: string;
  watchlist_id: string;
  candidate_id: string;
  smt_id: string;
  rule_version: string;
  trigger: AlertTrigger;
  setup_status: string;
  invalidation_reason: ExpiryReason | null;
  symbol_set: string[];
  sweeper_symbol: string;
  trade_symbols: string[];
  candidate_direction: string;
  deterministic_score: number;
  c2_case: number;
  context_timeframe: string;
  comparison_timeframe: string;
  validation_timeframe: string;
  c2_candle_ts: number;
  c3_candle_ts: number | null;
  smt_k_candle_ts: number | null;
  context_pda_id: string | null;
  validation_kind: string | null;
  validation_symbol: string | null;
  validation_ts: number | null;
  validation_direction: string | null;
  channels_fired: ChannelKind[];
  created_at: number;
};
