use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::candidate::{LtfEventRef, PackedEvidence};
use crate::detector::types::{
    CandleRef, Direction, IctStructure, LiquidityRef, LiquidityRefStatus, LiquiditySide, PdaRef,
    SmtDetectionState, SmtDivergence, StrengthLabel, SymbolChain,
};
use crate::types::{Bar, Timeframe};

use super::{
    DecisionQualityRubric, DeterministicDecisionGuardrails, LlmDecisionDirection,
    LlmDecisionQuality, LlmDecisionRequest, LlmEntryZone, LlmTarget, M7D_CONTEXT_VERSION,
    M7D_PROMPT_VERSION, M7D_STRATEGY_VERSION,
};

pub const DEFAULT_BARS_PER_TF: [usize; 3] = [100, 100, 120];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextIdentity {
    pub candidate_id: String,
    pub trade_symbol: String,
    pub watchlist_id: String,
    pub alert_id: String,
    pub as_of_ts: i64,
    pub prompt_version: String,
    pub strategy_version: String,
    pub context_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptConstraints {
    pub structured_data_only: bool,
    pub do_not_invent_structures: bool,
    pub cite_structure_ids: bool,
    pub insufficient_context_means_alert_false: bool,
    pub natural_language_output: String,
}

impl Default for PromptConstraints {
    fn default() -> Self {
        Self {
            structured_data_only: true,
            do_not_invent_structures: true,
            cite_structure_ids: true,
            insufficient_context_means_alert_false: true,
            natural_language_output: "zh-CN".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ContextBar {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BarSeries {
    pub symbol: String,
    pub timeframe: Timeframe,
    pub bars: Vec<ContextBar>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainSnapshot {
    pub symbol: String,
    pub c1: CandleRef,
    pub smt_k: CandleRef,
    pub c2: Option<CandleRef>,
    pub c2_case: Option<u8>,
    pub c3: Option<CandleRef>,
    pub state_as_of: SmtDetectionState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct L2StrategyReading {
    pub candidate_id: String,
    pub smt_id: String,
    pub watchlist_id: String,
    pub sweeper_symbol: String,
    pub trade_symbol: String,
    pub relationship: String,
    pub candidate_direction: String,
    pub effective_trade_direction: String,
    pub context_timeframe: Timeframe,
    pub comparison_timeframe: Timeframe,
    pub validation_timeframe: Timeframe,
    pub observation_window: (i64, i64),
    pub pda: Option<PdaRef>,
    pub liquidity_refs: Vec<LiquidityRef>,
    pub strength: Vec<StrengthLabel>,
    pub chains: Vec<ChainSnapshot>,
    pub ltf_reversals: Vec<LtfEventRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StructureEvidence {
    pub structure_id: String,
    pub kind: String,
    pub symbol: String,
    pub timeframe: Timeframe,
    pub available_at_ts: i64,
    pub state_as_of: String,
    pub facts: Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FacetContext {
    pub l2: Value,
    pub structure_ids: Vec<String>,
    pub structures: Vec<StructureEvidence>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ContextFacets {
    pub htf_bias: FacetContext,
    pub liquidity: FacetContext,
    pub entry_zone: FacetContext,
    pub smt: FacetContext,
    pub premium_discount: FacetContext,
    pub po3: FacetContext,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmDecisionContext {
    pub identity: ContextIdentity,
    pub prompt_constraints: PromptConstraints,
    pub l2_strategy_reading: L2StrategyReading,
    pub closed_bars: Vec<BarSeries>,
    pub facets: ContextFacets,
    #[serde(default)]
    pub deterministic_guardrails: DeterministicDecisionGuardrails,
}

#[derive(Clone, Debug)]
pub struct ContextPacker {
    bars_per_tf: [usize; 3],
}

impl Default for ContextPacker {
    fn default() -> Self {
        Self {
            bars_per_tf: DEFAULT_BARS_PER_TF,
        }
    }
}

impl ContextPacker {
    pub fn new(bars_per_tf: [usize; 3]) -> Self {
        Self { bars_per_tf }
    }

    pub fn bars_per_tf(&self) -> [usize; 3] {
        self.bars_per_tf
    }

    /// Pure deterministic packer. It reads no clock, storage, environment or
    /// network state. Equal inputs always serialize to byte-identical JSON.
    pub fn pack(&self, evidence: &PackedEvidence) -> Result<LlmDecisionContext, String> {
        let trade_symbol = evidence
            .trade_symbol
            .as_deref()
            .ok_or_else(|| "missing trade_symbol".to_string())?;
        self.pack_as_of(evidence, canonical_as_of_ts(evidence, trade_symbol))
    }

    /// Explicit historical market boundary. Offline callers must not advance
    /// an alert's timestamp to a later C2 found in today's persisted snapshot.
    /// Uses exactly the same normalization and guardrail formula as `pack`.
    pub fn pack_as_of(
        &self,
        evidence: &PackedEvidence,
        as_of_ts: i64,
    ) -> Result<LlmDecisionContext, String> {
        let trade_symbol = evidence
            .trade_symbol
            .clone()
            .ok_or_else(|| "missing trade_symbol".to_string())?;
        let alert_id = evidence.alert_id.clone().unwrap_or_default();
        let identity = ContextIdentity {
            candidate_id: evidence.candidate_id.clone(),
            trade_symbol: trade_symbol.clone(),
            watchlist_id: evidence.watchlist_id.clone(),
            alert_id,
            as_of_ts,
            prompt_version: M7D_PROMPT_VERSION.into(),
            strategy_version: M7D_STRATEGY_VERSION.into(),
            context_version: M7D_CONTEXT_VERSION.into(),
        };

        let chains = evidence
            .smt
            .chains
            .iter()
            .filter(|chain| {
                chain.symbol == evidence.smt.sweeper_symbol || chain.symbol == trade_symbol
            })
            .map(|chain| snapshot_chain(chain, evidence.smt.comparison_timeframe, as_of_ts))
            .collect();

        let ltf_reversals = ltf_validation_window(&evidence.smt, &trade_symbol, as_of_ts)
            .map(|(window_start, window_end)| {
                evidence
                    .ltf_cisd_mss
                    .iter()
                    .filter(|event| {
                        event.symbol == trade_symbol
                            && event.ts >= window_start
                            && event.ts < window_end
                            && event.ts <= as_of_ts
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        let effective_trade_direction = crate::candidate::effective_ltf_dir(
            evidence.smt.candidate_direction,
            &trade_symbol,
            &evidence.smt.sweeper_symbol,
            evidence.smt.relationship,
        );
        let l2_strategy_reading = L2StrategyReading {
            candidate_id: evidence.candidate_id.clone(),
            smt_id: evidence.smt.id.clone(),
            watchlist_id: evidence.watchlist_id.clone(),
            sweeper_symbol: evidence.smt.sweeper_symbol.clone(),
            trade_symbol: trade_symbol.clone(),
            relationship: format!("{:?}", evidence.smt.relationship).to_ascii_lowercase(),
            candidate_direction: format!("{:?}", evidence.smt.candidate_direction)
                .to_ascii_lowercase(),
            effective_trade_direction: format!("{effective_trade_direction:?}")
                .to_ascii_lowercase(),
            context_timeframe: evidence.smt.context_timeframe,
            comparison_timeframe: evidence.smt.comparison_timeframe,
            validation_timeframe: evidence.candidate.validation_timeframe,
            observation_window: (
                evidence.smt.observation_window.0.min(as_of_ts),
                evidence.smt.observation_window.1.min(as_of_ts),
            ),
            pda: snapshot_pda(evidence.smt.htf_pda_ref.as_ref(), as_of_ts),
            liquidity_refs: evidence
                .smt
                .liquidity_refs
                .iter()
                .filter(|reference| reference.ref_ts <= as_of_ts)
                .map(|reference| snapshot_liquidity_ref(reference, as_of_ts))
                .collect(),
            strength: evidence.smt.strength.clone(),
            chains,
            ltf_reversals,
        };

        let symbols = [evidence.smt.sweeper_symbol.as_str(), trade_symbol.as_str()];
        let tfs = [
            evidence.smt.context_timeframe,
            evidence.smt.comparison_timeframe,
            evidence.candidate.validation_timeframe,
        ];
        let mut closed_bars = Vec::with_capacity(6);
        for symbol in symbols {
            for (index, tf) in tfs.iter().copied().enumerate() {
                closed_bars.push(pack_bars(
                    &evidence.market_bars,
                    symbol,
                    tf,
                    as_of_ts,
                    self.bars_per_tf[index],
                ));
            }
        }

        let mut facets = ContextFacets::default();
        facets.htf_bias.l2 = json!({
            "candidate_direction": l2_strategy_reading.candidate_direction,
            "effective_trade_direction": l2_strategy_reading.effective_trade_direction,
            "context_timeframe": l2_strategy_reading.context_timeframe,
            "strength": l2_strategy_reading.strength,
        });
        facets.liquidity.l2 = json!({
            "references": l2_strategy_reading.liquidity_refs,
        });
        facets.entry_zone.l2 = json!({
            "pda": l2_strategy_reading.pda,
            "effective_trade_direction": l2_strategy_reading.effective_trade_direction,
            "ltf_reversals": l2_strategy_reading.ltf_reversals,
        });
        facets.smt.l2 = json!({
            "smt_id": l2_strategy_reading.smt_id,
            "relationship": l2_strategy_reading.relationship,
            "observation_window": l2_strategy_reading.observation_window,
            "chains": l2_strategy_reading.chains,
        });
        facets
            .smt
            .structure_ids
            .push(l2_strategy_reading.smt_id.clone());
        facets.premium_discount.l2 = json!({
            "effective_trade_direction": l2_strategy_reading.effective_trade_direction,
        });
        facets.po3.l2 = json!({
            "effective_trade_direction": l2_strategy_reading.effective_trade_direction,
        });
        let allowed_symbols: BTreeSet<&str> = symbols.into_iter().collect();
        let allowed_tfs: BTreeSet<Timeframe> = tfs.into_iter().collect();
        let mut structures: Vec<_> = evidence
            .market_structures
            .iter()
            .filter(|structure| {
                allowed_symbols.contains(structure.symbol())
                    && (allowed_tfs.contains(&structure.tf())
                        || matches!(structure, IctStructure::Pdh(_) | IctStructure::Pdl(_)))
            })
            .filter_map(|structure| normalize_structure(structure, as_of_ts))
            .collect();
        structures.sort_by(|a, b| {
            (
                a.available_at_ts,
                &a.symbol,
                a.timeframe,
                &a.kind,
                &a.structure_id,
            )
                .cmp(&(
                    b.available_at_ts,
                    &b.symbol,
                    b.timeframe,
                    &b.kind,
                    &b.structure_id,
                ))
        });
        structures.dedup_by(|a, b| a.structure_id == b.structure_id);
        for structure in structures {
            for facet in facet_targets(&structure.kind) {
                let target = match *facet {
                    "htf_bias" => &mut facets.htf_bias,
                    "liquidity" => &mut facets.liquidity,
                    "entry_zone" => &mut facets.entry_zone,
                    "smt" => &mut facets.smt,
                    "premium_discount" => &mut facets.premium_discount,
                    "po3" => &mut facets.po3,
                    _ => continue,
                };
                target.structure_ids.push(structure.structure_id.clone());
                target.structures.push(structure.clone());
            }
        }

        let deterministic_guardrails = build_decision_guardrails(
            evidence,
            &trade_symbol,
            effective_trade_direction,
            &l2_strategy_reading,
            &facets,
            as_of_ts,
        );

        Ok(LlmDecisionContext {
            identity,
            prompt_constraints: PromptConstraints::default(),
            l2_strategy_reading,
            closed_bars,
            facets,
            deterministic_guardrails,
        })
    }

    pub fn request(&self, evidence: &PackedEvidence) -> Result<LlmDecisionRequest, String> {
        let mut request = LlmDecisionRequest {
            context: self.pack(evidence)?,
        };
        request.bound_provider_context();
        Ok(request)
    }
}

fn build_decision_guardrails(
    evidence: &PackedEvidence,
    trade_symbol: &str,
    effective_direction: Direction,
    l2: &L2StrategyReading,
    facets: &ContextFacets,
    as_of_ts: i64,
) -> DeterministicDecisionGuardrails {
    let rubric = DecisionQualityRubric::default();
    let direction = match effective_direction {
        Direction::Bullish => LlmDecisionDirection::Bullish,
        Direction::Bearish => LlmDecisionDirection::Bearish,
    };
    let trade_chain = l2.chains.iter().find(|chain| chain.symbol == trade_symbol);
    let entry_zone = trade_chain
        .and_then(|chain| chain.c2.as_ref())
        .map(|c2| LlmEntryZone {
            low: truncate_price(trade_symbol, c2.open.min(c2.close)),
            high: truncate_price(trade_symbol, c2.open.max(c2.close)),
            source: format!("program:C2_body:{trade_symbol}:{}", c2.ts),
        });
    let invalidation_price = trade_chain.and_then(|chain| {
        let c2 = chain.c2.as_ref()?;
        let mut adverse = match effective_direction {
            Direction::Bullish => chain.smt_k.low.min(c2.low),
            Direction::Bearish => chain.smt_k.high.max(c2.high),
        };
        if let Some(c3) = chain.c3.as_ref() {
            adverse = match effective_direction {
                Direction::Bullish => adverse.min(c3.low),
                Direction::Bearish => adverse.max(c3.high),
            };
        }
        Some(truncate_price(trade_symbol, adverse))
    });

    let mut targets: Vec<LlmTarget> = l2
        .liquidity_refs
        .iter()
        .filter(|reference| reference.symbol == trade_symbol)
        .filter(|reference| {
            matches!(
                reference.status,
                LiquidityRefStatus::NotSwept | LiquidityRefStatus::EqualHighLow
            )
        })
        .filter(|reference| match effective_direction {
            Direction::Bullish => reference.side == LiquiditySide::BuySide,
            Direction::Bearish => reference.side == LiquiditySide::SellSide,
        })
        .filter(|reference| {
            entry_zone
                .as_ref()
                .is_some_and(|zone| match effective_direction {
                    Direction::Bullish => reference.ref_price > zone.high,
                    Direction::Bearish => reference.ref_price < zone.low,
                })
        })
        .map(|reference| LlmTarget {
            price: truncate_price(trade_symbol, reference.ref_price),
            reason: format!(
                "program:未取走的{:?}流动性:{:?}:{}",
                reference.side, reference.tf, reference.ref_ts
            ),
        })
        .collect();
    if let Some(zone) = entry_zone.as_ref() {
        targets.extend(super::liquidity_targets::collect(
            evidence,
            trade_symbol,
            effective_direction,
            zone,
            &facets.liquidity,
            as_of_ts,
        ));
    }
    // Rounding must not turn an outside target into the entry boundary.
    targets.retain(|target| {
        target.price.is_finite()
            && entry_zone
                .as_ref()
                .is_some_and(|zone| match effective_direction {
                    Direction::Bullish => target.price > zone.high,
                    Direction::Bearish => target.price < zone.low,
                })
    });
    targets.sort_by(|left, right| {
        left.price
            .partial_cmp(&right.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if effective_direction == Direction::Bearish {
        targets.reverse();
    }
    targets.dedup_by(|left, right| left.price == right.price);
    targets.truncate(3);

    let risk_reward =
        entry_zone
            .as_ref()
            .zip(invalidation_price)
            .and_then(|(zone, invalidation)| {
                let entry = (zone.low + zone.high) / 2.0;
                let target = targets.first()?.price;
                let risk = (entry - invalidation).abs();
                let reward = (target - entry).abs();
                let valid_geometry = zone.low.is_finite()
                    && zone.high.is_finite()
                    && zone.low <= zone.high
                    && invalidation.is_finite()
                    && target.is_finite()
                    && match effective_direction {
                        Direction::Bullish => invalidation < zone.low && target > zone.high,
                        Direction::Bearish => invalidation > zone.high && target < zone.low,
                    };
                (valid_geometry && risk > f64::EPSILON && reward > 0.0)
                    .then(|| truncate(reward / risk, 4))
            });

    let valid_price_relation =
        entry_zone
            .as_ref()
            .zip(invalidation_price)
            .is_some_and(|(zone, invalidation)| match effective_direction {
                Direction::Bullish => invalidation < zone.low,
                Direction::Bearish => invalidation > zone.high,
            });
    let mut confidence =
        (evidence.candidate.deterministic_score.clamp(0.0, 1.0) * 100.0).round() as u8;
    let mut should_wait_for = Vec::new();
    let mut warnings = vec!["LLM 输出仅作人工交易决策辅助，不得自动下单".into()];
    if entry_zone.is_none() || invalidation_price.is_none() || !valid_price_relation {
        confidence = confidence.min(rubric.quality_c_min_confidence.saturating_sub(1));
        should_wait_for.push("等待可验证的 C2 入场区与方向一致的失效位".into());
    }
    if targets.is_empty() || risk_reward.is_none() {
        confidence = confidence.min(rubric.quality_b_min_confidence.saturating_sub(1));
        warnings.push("上下文内没有可确定计算的顺向未取流动性目标，目标位与盈亏比留空".into());
    }
    let quality = quality_for_confidence(confidence, &rubric);
    let alert_eligible = confidence >= rubric.alert_min_confidence
        && entry_zone.is_some()
        && invalidation_price.is_some()
        && valid_price_relation;

    let mut allowed_evidence_ids = all_facet_structure_ids(facets);
    allowed_evidence_ids.push(l2.smt_id.clone());
    if let Some(pda) = l2.pda.as_ref() {
        allowed_evidence_ids.push(pda.id.clone());
    }
    allowed_evidence_ids.extend(l2.ltf_reversals.iter().map(|event| event.id.clone()));
    allowed_evidence_ids.sort();
    allowed_evidence_ids.dedup();
    let mut required_evidence_ids = vec![l2.smt_id.clone()];
    if let Some(pda) = l2.pda.as_ref() {
        required_evidence_ids.push(pda.id.clone());
    }

    DeterministicDecisionGuardrails {
        rule_version: M7D_STRATEGY_VERSION.into(),
        direction,
        confidence,
        quality,
        alert_eligible,
        entry_zone,
        invalidation_price,
        targets,
        risk_reward,
        required_evidence_ids,
        allowed_evidence_ids,
        should_wait_for,
        warnings,
        calculation_notes: vec![
            "方向取 effective_trade_direction，不允许模型改写".into(),
            "入场区取交易品种已收盘 C2 实体范围".into(),
            "失效位取 SMT K/C2/已收盘 C3 的不利极值".into(),
            "目标来源=SMT参照 + 交易品种L1 PDH/PDL/已确认等高等低；顺向、未扫、区间外、近者优先≤3".into(),
            "L1未扫状态要求从可用时点至as_of有完整已收盘价格覆盖；覆盖不足或旧等高等低无确认时间则不猜测".into(),
            "置信度取 deterministic_score，并按证据完整度封顶；A>=80、B>=65、C>=50、否则 Skip"
                .into(),
        ],
        quality_rubric: rubric,
    }
}

fn quality_for_confidence(confidence: u8, rubric: &DecisionQualityRubric) -> LlmDecisionQuality {
    if confidence >= rubric.quality_a_min_confidence {
        LlmDecisionQuality::A
    } else if confidence >= rubric.quality_b_min_confidence {
        LlmDecisionQuality::B
    } else if confidence >= rubric.quality_c_min_confidence {
        LlmDecisionQuality::C
    } else {
        LlmDecisionQuality::Skip
    }
}

fn all_facet_structure_ids(facets: &ContextFacets) -> Vec<String> {
    [
        &facets.htf_bias,
        &facets.liquidity,
        &facets.entry_zone,
        &facets.smt,
        &facets.premium_discount,
        &facets.po3,
    ]
    .into_iter()
    .flat_map(|facet| facet.structure_ids.iter().cloned())
    .collect()
}

pub(super) fn canonical_as_of_ts(evidence: &PackedEvidence, trade_symbol: &str) -> i64 {
    let c2_ts = |symbol: &str| {
        evidence
            .smt
            .chains
            .iter()
            .find(|chain| chain.symbol == symbol)
            .and_then(|chain| chain.c2_candle.as_ref())
            .map(|candle| {
                candle
                    .ts
                    .saturating_add(evidence.smt.comparison_timeframe.duration_ms())
            })
    };
    match (c2_ts(&evidence.smt.sweeper_symbol), c2_ts(trade_symbol)) {
        (Some(left), Some(right)) => left.max(right),
        (Some(ts), None) | (None, Some(ts)) => ts,
        (None, None) => evidence.as_of_ts,
    }
}

/// Return the trade symbol's strategy-valid reversal window as it was known
/// at `as_of_ts`. A future C3 must not expand the window early: before C3 has
/// closed, only the completed C2 MTF candle is eligible.
fn ltf_validation_window(
    smt: &SmtDivergence,
    trade_symbol: &str,
    as_of_ts: i64,
) -> Option<(i64, i64)> {
    let chain = smt
        .chains
        .iter()
        .find(|chain| chain.symbol == trade_symbol)?;
    let duration = smt.comparison_timeframe.duration_ms();
    let c2 = chain.c2_candle.as_ref()?;
    if c2.ts.saturating_add(duration) > as_of_ts {
        return None;
    }
    let last_visible_window_ts = chain
        .c3_candle
        .as_ref()
        .filter(|c3| c3.ts.saturating_add(duration) <= as_of_ts)
        .map_or(c2.ts, |c3| c3.ts);
    Some((c2.ts, last_visible_window_ts.saturating_add(duration)))
}

fn snapshot_chain(chain: &SymbolChain, tf: Timeframe, as_of_ts: i64) -> ChainSnapshot {
    let is_closed = |candle: &CandleRef| candle.ts.saturating_add(tf.duration_ms()) <= as_of_ts;
    let c2 = chain.c2_candle.as_ref().filter(|c| is_closed(c)).cloned();
    let c3 = chain.c3_candle.as_ref().filter(|c| is_closed(c)).cloned();
    let state_as_of = if c3.is_some() {
        SmtDetectionState::C3Entry
    } else if c2.is_some() {
        SmtDetectionState::C2Confirmed
    } else {
        SmtDetectionState::SmtKDetected
    };
    ChainSnapshot {
        symbol: chain.symbol.clone(),
        c1: chain.c1_candle.clone(),
        smt_k: chain.smt_k_candle.clone(),
        c2_case: c2.as_ref().and(chain.c2_case),
        c2,
        c3,
        state_as_of,
    }
}

fn snapshot_pda(pda: Option<&PdaRef>, as_of_ts: i64) -> Option<PdaRef> {
    let pda = pda?.clone();
    if pda.ts_confirm > as_of_ts || pda.ts_filled.is_some_and(|ts| ts <= as_of_ts) {
        return None;
    }
    Some(PdaRef {
        exit_ts: pda.exit_ts.filter(|ts| *ts <= as_of_ts),
        ts_filled: pda.ts_filled.filter(|ts| *ts <= as_of_ts),
        ..pda
    })
}

fn snapshot_liquidity_ref(reference: &LiquidityRef, as_of_ts: i64) -> LiquidityRef {
    let mut snapshot = reference.clone();
    snapshot.mtf_ref_candle = snapshot
        .mtf_ref_candle
        .filter(|candle| candle.ts <= as_of_ts);
    snapshot.mtf_sweep_candle = snapshot
        .mtf_sweep_candle
        .filter(|candle| candle.ts <= as_of_ts);
    snapshot
}

fn pack_bars(bars: &[Bar], symbol: &str, tf: Timeframe, as_of_ts: i64, limit: usize) -> BarSeries {
    let mut unique = BTreeMap::new();
    for bar in bars.iter().filter(|bar| {
        bar.symbol == symbol && bar.tf == tf && bar.ts.saturating_add(tf.duration_ms()) <= as_of_ts
    }) {
        unique.insert(bar.ts, bar);
    }
    let skip = unique.len().saturating_sub(limit);
    let bars = unique
        .into_values()
        .skip(skip)
        .map(|bar| ContextBar {
            ts: bar.ts,
            open: truncate_price(symbol, bar.open),
            high: truncate_price(symbol, bar.high),
            low: truncate_price(symbol, bar.low),
            close: truncate_price(symbol, bar.close),
            volume: truncate(bar.volume, 2),
        })
        .collect();
    BarSeries {
        symbol: symbol.into(),
        timeframe: tf,
        bars,
    }
}

pub fn price_decimals(symbol: &str) -> u32 {
    let symbol = symbol.to_ascii_uppercase();
    if symbol.contains("BTC") {
        2
    } else if symbol.contains("DXY") {
        3
    } else if symbol.contains("JPY") || symbol.contains("XAU") || symbol.contains("XAG") {
        3
    } else {
        5
    }
}

pub(super) fn truncate_price(symbol: &str, value: f64) -> f64 {
    truncate(value, price_decimals(symbol))
}

fn truncate(value: f64, decimals: u32) -> f64 {
    let factor = 10_f64.powi(decimals as i32);
    (value * factor).trunc() / factor
}

fn normalize_structure(structure: &IctStructure, as_of_ts: i64) -> Option<StructureEvidence> {
    if matches!(structure, IctStructure::SmtDivergence(_)) {
        return None;
    }
    let available_at_ts = structure_available_at(structure)?;
    if available_at_ts > as_of_ts {
        return None;
    }
    let mut facts = serde_json::to_value(structure).ok()?;
    scrub_future_fields(&mut facts, as_of_ts);
    let future_fill = matches!(
        structure,
        IctStructure::Fvg(fvg) if fvg.ts_filled.is_some_and(|ts| ts > as_of_ts)
    );
    let state_as_of = if future_fill {
        if let Value::Object(map) = &mut facts {
            map.insert("state".into(), Value::String("unknown_pre_fill".into()));
        }
        "unknown_pre_fill"
    } else {
        structure.state_tag()
    }
    .to_string();
    Some(StructureEvidence {
        structure_id: structure.id().into(),
        kind: structure.kind_tag().into(),
        symbol: structure.symbol().into(),
        timeframe: structure.tf(),
        available_at_ts,
        state_as_of,
        facts,
    })
}

fn structure_available_at(structure: &IctStructure) -> Option<i64> {
    Some(match structure {
        IctStructure::Fvg(s) => s.ts_confirm,
        IctStructure::OrderBlock(s) => s.ts_confirm,
        IctStructure::Mss(s) => s.break_ts,
        IctStructure::Cisd(s) => s.break_ts,
        // Historical rollover markers use a display window; force-emit
        // markers use the source-day window. Prefer the explicit close fact.
        // source_ts disambiguates both legacy producers; missing provenance
        // falls back to the later boundary, never guesses early availability.
        IctStructure::Pdh(s) | IctStructure::Pdl(s) => s.confirmed_at_ts.unwrap_or_else(|| {
            if s.source_ts.is_some_and(|ts| ts < s.valid_from_ts) {
                s.valid_from_ts
            } else {
                s.valid_until_ts
            }
        }),
        IctStructure::KillZone(s) => s.ts_start,
        IctStructure::LiquiditySweep(s) => s.sweep_ts.saturating_add(s.tf.duration_ms()),
        // A pivot timestamp alone cannot establish when the fractal was
        // confirmed. Unknown legacy availability is excluded from both
        // numeric guardrails and LLM context, not just from target selection.
        IctStructure::EqualHighsLows(s) => s.confirmed_at_ts?,
        IctStructure::LiquidityReversal(s) => s.confirm_ts,
        IctStructure::BreakerBlock(s) => s.ts_confirm,
        IctStructure::Bos(s) => s.break_ts,
        IctStructure::VolumeImbalance(s) => s.ts_confirm,
        IctStructure::Ote(s) => s.leg_end_ts,
        IctStructure::PremiumDiscount(s) => s.range_end_ts,
        IctStructure::Nwog(s) | IctStructure::Ndog(s) => s.new_open_ts,
        IctStructure::SessionRange(s) => s.ts_end,
        IctStructure::KillZoneWindow(s) => s.ts_start,
        IctStructure::PowerOf3(s) => s.confirm_ts,
        IctStructure::SmtDivergence(_) => return None,
    })
}

fn scrub_future_fields(value: &mut Value, as_of_ts: i64) {
    match value {
        Value::Object(map) => {
            let future_keys: Vec<String> = map
                .iter()
                .filter_map(|(key, value)| {
                    let is_timestamp =
                        key == "ts" || key.ends_with("_ts") || key.starts_with("ts_");
                    (is_timestamp && value.as_i64().is_some_and(|ts| ts > as_of_ts))
                        .then(|| key.clone())
                })
                .collect();
            for key in future_keys {
                map.remove(&key);
            }
            for child in map.values_mut() {
                scrub_future_fields(child, as_of_ts);
            }
        }
        Value::Array(items) => {
            for item in items {
                scrub_future_fields(item, as_of_ts);
            }
        }
        _ => {}
    }
}

fn facet_targets(kind: &str) -> &'static [&'static str] {
    match kind {
        "mss" | "cisd" | "bos" => &["htf_bias", "entry_zone"],
        "fvg" | "order_block" | "breaker_block" | "volume_imbalance" | "ote" => &["entry_zone"],
        "pdh" | "pdl" | "liquidity_sweep" | "equal_highs_lows" | "liquidity_reversal" => {
            &["liquidity"]
        }
        "premium_discount" => &["premium_discount", "htf_bias"],
        "power_of_3" => &["po3"],
        "nwog" | "ndog" | "session_range" | "kill_zone" | "kill_zone_window" => &["htf_bias"],
        _ => &[],
    }
}

/// Bound only the LLM request, after deterministic calculations use full evidence.
/// Offline pack()/pack_as_of() keep their original evaluation semantics.
impl LlmDecisionRequest {
    pub fn bound_provider_context(&mut self) {
        let c = &mut self.context;
        let before = serde_json::to_vec(&c.facets).map(|v| v.len()).unwrap_or(0);
        let mut pinned: BTreeSet<String> = c
            .deterministic_guardrails
            .required_evidence_ids
            .iter()
            .cloned()
            .collect();
        pinned.insert(c.l2_strategy_reading.smt_id.clone());
        if let Some(pda) = &c.l2_strategy_reading.pda {
            pinned.insert(pda.id.clone());
        }
        pinned.extend(
            c.l2_strategy_reading
                .ltf_reversals
                .iter()
                .map(|v| v.id.clone()),
        );
        let targets = &c.deterministic_guardrails.targets;
        let mut retained = pinned.clone();
        for facet in [
            &mut c.facets.htf_bias,
            &mut c.facets.liquidity,
            &mut c.facets.entry_zone,
            &mut c.facets.smt,
            &mut c.facets.premium_discount,
            &mut c.facets.po3,
        ] {
            // Pick recent evidence across symbol/timeframe/kind groups before filling
            // remaining slots. Critical PDA/reversal/target references are never evicted.
            facet.structures.sort_by(|a, b| {
                b.available_at_ts
                    .cmp(&a.available_at_ts)
                    .then(a.structure_id.cmp(&b.structure_id))
            });
            let mut selected = BTreeSet::new();
            let mut groups = BTreeSet::new();
            for v in &facet.structures {
                if pinned.contains(&v.structure_id)
                    || targets
                        .iter()
                        .any(|t| t.reason.contains(&format!(":{}:", v.structure_id)))
                {
                    selected.insert(v.structure_id.clone());
                }
            }
            for v in &facet.structures {
                if selected.len() >= 16 {
                    break;
                }
                if groups.insert((v.symbol.clone(), v.timeframe, v.kind.clone())) {
                    selected.insert(v.structure_id.clone());
                }
            }
            for v in &facet.structures {
                if selected.len() >= 16 {
                    break;
                }
                selected.insert(v.structure_id.clone());
            }
            facet
                .structures
                .retain(|v| selected.contains(&v.structure_id));
            facet
                .structure_ids
                .retain(|id| selected.contains(id) || pinned.contains(id));
            retained.extend(facet.structure_ids.iter().cloned());
        }
        c.deterministic_guardrails
            .allowed_evidence_ids
            .retain(|id| retained.contains(id));
        c.identity.context_version = format!("{M7D_CONTEXT_VERSION}.bounded-v1");
        let after = serde_json::to_vec(&c.facets).map(|v| v.len()).unwrap_or(0);
        if after < before {
            c.deterministic_guardrails.calculation_notes.push(
                "LLM背景结构已限量：各分面优先保留关键引用及近期结构；确定性价位使用完整证据计算。"
                    .into(),
            );
        }
    }
}
