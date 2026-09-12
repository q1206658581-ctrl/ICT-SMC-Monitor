import type { Tf } from '../store';

export type LlmDecisionStatus = 'pending' | 'approved' | 'rejected' | 'error';
export type LlmDecisionDirection = 'bullish' | 'bearish' | 'neutral';
export type LlmDecisionQuality = 'A' | 'B' | 'C' | 'skip';

export interface LlmEntryZone {
  low: number;
  high: number;
  source: string;
}

export interface LlmTarget {
  price: number;
  reason: string;
}

export interface LlmDecision {
  candidate_id: string;
  alert: boolean;
  direction: LlmDecisionDirection;
  confidence: number;
  quality: LlmDecisionQuality;
  reasoning_summary: string;
  evidence_structure_ids: string[];
  invalidation_price: number | null;
  entry_zone: LlmEntryZone | null;
  targets: LlmTarget[];
  risk_reward: number | null;
  should_wait_for: string[];
  warnings: string[];
}

/** Safe product projection. Audit prompts and raw model responses are omitted. */
export interface LlmDecisionListItem {
  id: string;
  candidate_id: string;
  watchlist_id: string;
  alert_id: string | null;
  trade_symbol: string | null;
  market_anchor_ts: number;
  validation_timeframe: Tf | null;
  provider: string;
  model: string | null;
  created_at: number;
  status: LlmDecisionStatus;
  decision: LlmDecision | null;
  error_summary: string | null;
}
