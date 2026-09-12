use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::detector::atr::{atr14, pip_size};
use crate::types::{Bar, Timeframe};

use super::types::{
    structure_id, Bos, Direction, Fvg, FvgState, GapState, IctStructure, ObState, PdSide,
    Po3ContextKind, Po3Stage, Po3StageBox, Po3State, PowerOf3, ReversalConfirmKind, SessionKind,
    StructureEvent, ZoneState,
};
use super::{Detector, DetectorCtx};

const DISTRIBUTION_BOX_BARS: i64 = 6;
const DISTRIBUTION_ACCEPTANCE_BARS: usize = 6;

#[derive(Clone, Debug)]
pub struct Po3Config {
    pub enabled: bool,
    pub min_accumulation_bars: usize,
    pub max_accumulation_bars: usize,
    pub max_range_atr_mult: f64,
    pub require_liquidity_pool: bool,
    pub max_bars_after_sweep: usize,
    pub min_quality_score: u8,
    pub allow_cisd: bool,
    pub allow_mss: bool,
}

impl Default for Po3Config {
    fn default() -> Self {
        Self {
            enabled: true,
            min_accumulation_bars: 8,
            max_accumulation_bars: 30,
            max_range_atr_mult: 1.2,
            require_liquidity_pool: true,
            max_bars_after_sweep: 10,
            min_quality_score: 4,
            allow_cisd: true,
            allow_mss: true,
        }
    }
}

#[derive(Clone, Debug)]
struct AccumulationCandidate {
    start_ts: i64,
    end_ts: i64,
    high: f64,
    low: f64,
    has_eq: bool,
    has_touches: bool,
}

#[derive(Clone, Debug)]
struct PendingSweep {
    id: String,
    accumulation: AccumulationCandidate,
    direction: Direction,
    sweep_ts: i64,
    sweep_price: f64,
    bars_waited: usize,
    context: ContextMatch,
    base_score: u8,
    state: Po3State,
    entry: Option<EntryHit>,
}

#[derive(Clone, Debug)]
struct PendingConfirmedPo3 {
    sweep: PendingSweep,
    confirm: ConfirmHit,
    score: u8,
    id: String,
    bars_waited: usize,
}

#[derive(Clone, Debug)]
struct EntryHit {
    id: String,
    tf: Timeframe,
    ts: i64,
    price: f64,
    kind: ReversalConfirmKind,
}

#[derive(Clone, Debug, Default)]
struct ContextMatch {
    ids: Vec<String>,
    tfs: Vec<Timeframe>,
    directional_score: u8,
    pd_score: u8,
    neutral_score: u8,
    primary: Option<Po3ContextKind>,
    has_directional_gate: bool,
}

pub struct Po3Detector {
    symbol: String,
    tf: Timeframe,
    cfg: Po3Config,
    pending: Vec<PendingSweep>,
    confirmed: Vec<PendingConfirmedPo3>,
    emitted: HashMap<String, PowerOf3>,
    emitted_range_keys: HashSet<String>,
}

impl Po3Detector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: Po3Config) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            pending: Vec::new(),
            confirmed: Vec::new(),
            emitted: HashMap::new(),
            emitted_range_keys: HashSet::new(),
        }
    }
    fn sync_existing(&mut self, structures: &[IctStructure], now_ts: i64) {
        for s in structures {
            if let IctStructure::PowerOf3(po3) = s {
                if po3.symbol == self.symbol && po3.tf == self.tf {
                    self.emitted.insert(po3.id.clone(), po3.clone());
                    // Reconstruct pending/confirmed state so the detector
                    // can continue processing hydrated PO3 structures on
                    // live bars (check for confirmation or invalidate).
                    let pending_has = self.pending.iter().any(|p| p.id == po3.id);
                    let confirmed_has = self.confirmed.iter().any(|c| c.id == po3.id);
                    let range_locked = self.emitted_range_keys.contains(&po3.id);
                    if !pending_has && !confirmed_has && !range_locked {
                        self.rehydrate_po3(po3, now_ts);
                    }
                }
            }
        }
    }

    /// Reconstruct internal pending/confirmed state from a hydrated
    /// PowerOf3 structure so live bars can continue processing it.
    fn rehydrate_po3(&mut self, po3: &PowerOf3, _now_ts: i64) {
        let accumulation = AccumulationCandidate {
            start_ts: po3.accumulation_start_ts,
            end_ts: po3.accumulation_end_ts,
            high: po3.accumulation_high,
            low: po3.accumulation_low,
            has_eq: false,
            has_touches: false,
        };
        let entry = po3.entry_ts.map(|ts| EntryHit {
            id: po3.entry_id.clone().unwrap_or_default(),
            tf: po3.entry_tf.unwrap_or(self.tf),
            ts,
            price: po3.entry_price.unwrap_or(po3.sweep_price),
            kind: po3.entry_kind.unwrap_or(ReversalConfirmKind::Cisd),
        });
        let context = ContextMatch {
            ids: po3.context_structure_ids.clone(),
            tfs: po3.context_timeframes.clone(),
            directional_score: 0,
            pd_score: 0,
            neutral_score: 0,
            primary: Some(po3.context_kind),
            has_directional_gate: true,
        };
        let sweep = PendingSweep {
            id: po3.id.clone(),
            accumulation,
            direction: po3.direction,
            sweep_ts: po3.sweep_ts,
            sweep_price: po3.sweep_price,
            bars_waited: 0,
            context,
            base_score: po3
                .quality_score
                .saturating_sub(confirm_score(po3.confirm_kind)),
            state: po3.state,
            entry,
        };
        match po3.state {
            Po3State::ManipulationSwept | Po3State::EarlyReversal => {
                self.pending.push(sweep);
            }
            Po3State::ReversalConfirmed => {
                let confirm = ConfirmHit {
                    id: po3.confirm_id.clone(),
                    ts: po3.confirm_ts,
                    kind: po3.confirm_kind,
                };
                self.confirmed.push(PendingConfirmedPo3 {
                    sweep,
                    confirm,
                    score: po3.quality_score,
                    id: po3.id.clone(),
                    bars_waited: 0,
                });
            }
            Po3State::DistributionConfirmed => {
                // Already fully accepted - do NOT re-enter the confirmation
                // pipeline. Putting it in `self.confirmed` lets
                // `emit_distribution_acceptance` invalidate it after
                // DISTRIBUTION_ACCEPTANCE_BARS because the acceptance
                // condition won't re-match on seed bars that are far past
                // the distribution window. This was the root cause of PO3
                // disappearing on every restart. Keep it in `self.emitted`
                // (already inserted by `sync_existing`) and range-lock the
                // id so it is never re-emitted or invalidated.
                self.emitted_range_keys.insert(po3.id.clone());
            }
            Po3State::AccumulationCandidate => {}
        }
    }

    fn best_accumulation(
        &self,
        bars: &[Bar],
        structures: &[IctStructure],
    ) -> Option<AccumulationCandidate> {
        let last = bars.last()?;
        let atr = atr14(bars)?;
        if atr <= f64::EPSILON {
            return None;
        }
        let max_n = self
            .cfg
            .max_accumulation_bars
            .min(bars.len().saturating_sub(1));
        let min_n = self.cfg.min_accumulation_bars.max(2);
        if max_n < min_n {
            return None;
        }

        let mut best: Option<(i32, AccumulationCandidate)> = None;
        for n in min_n..=max_n {
            let end = bars.len().saturating_sub(1);
            let start = end.saturating_sub(n);
            let window = &bars[start..end];
            if window.len() != n {
                continue;
            }
            let mut high = f64::NEG_INFINITY;
            let mut low = f64::INFINITY;
            for b in window {
                if b.high > high {
                    high = b.high;
                }
                if b.low < low {
                    low = b.low;
                }
            }
            let range = high - low;
            if range <= f64::EPSILON || range > self.cfg.max_range_atr_mult.max(0.1) * atr {
                continue;
            }
            if last.ts <= window.last()?.ts {
                continue;
            }
            let touch_tolerance = touch_tolerance(&self.symbol, atr);
            let has_eq = has_eqh_eql_overlap(
                structures,
                &self.symbol,
                self.tf,
                window[0].ts,
                window[window.len() - 1].ts,
                low,
                high,
            );
            let has_touches = boundary_touches(window, low, high, touch_tolerance);
            if self.cfg.require_liquidity_pool && !has_eq && !has_touches {
                continue;
            }
            let narrow_score = ((1.0 - (range / (self.cfg.max_range_atr_mult.max(0.1) * atr)))
                .max(0.0)
                * 10.0) as i32;
            let score = narrow_score
                + if has_eq { 20 } else { 0 }
                + if has_touches { 10 } else { 0 }
                + n as i32;
            let candidate = AccumulationCandidate {
                start_ts: window[0].ts,
                end_ts: window[window.len() - 1].ts,
                high,
                low,
                has_eq,
                has_touches,
            };
            if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                best = Some((score, candidate));
            }
        }
        best.map(|(_, c)| c)
    }

    fn detect_sweep(
        &mut self,
        bar: &Bar,
        bars: &[Bar],
        candidate: AccumulationCandidate,
        ctx: &DetectorCtx<'_>,
        out: &mut Vec<StructureEvent>,
    ) {
        if bar.ts <= candidate.end_ts {
            return;
        }
        let range_key = accumulation_key(&self.symbol, self.tf, &candidate);
        let mut maybe_push = |direction: Direction, sweep_price: f64, this: &mut Self| {
            let context = context_match(
                &this.symbol,
                this.tf,
                direction,
                candidate.low,
                candidate.high,
                bar.ts,
                ctx.structures,
            );
            if !context.has_directional_gate {
                return;
            }
            let base_score = score_base(&candidate, &context);
            if base_score + 2 < this.cfg.min_quality_score {
                return;
            }
            if this.pending.iter().any(|p| {
                accumulation_key(&this.symbol, this.tf, &p.accumulation) == range_key
                    && p.direction == direction
            }) {
                return;
            }
            let id = po3_candidate_id(&this.symbol, this.tf, direction, &candidate, bar.ts);
            if this.emitted.contains_key(&id) || this.emitted_range_keys.contains(&id) {
                return;
            }
            let sweep = PendingSweep {
                id: id.clone(),
                accumulation: candidate.clone(),
                direction,
                sweep_ts: bar.ts,
                sweep_price,
                bars_waited: 0,
                context,
                base_score,
                state: Po3State::ManipulationSwept,
                entry: None,
            };
            let po3 = this.build_candidate_po3(&sweep, Po3State::ManipulationSwept, None, bar.ts);
            this.emitted.insert(id, po3.clone());
            out.push(StructureEvent::New(IctStructure::PowerOf3(po3)));
            this.pending.push(sweep);
        };
        if bar.low < candidate.low && bar.close > candidate.low {
            maybe_push(Direction::Bullish, bar.low, self);
        }
        if bar.high > candidate.high && bar.close < candidate.high {
            maybe_push(Direction::Bearish, bar.high, self);
        }
        self.emit_confirmations(bar, bars, ctx.structures, out);
    }

    fn build_candidate_po3(
        &self,
        sweep: &PendingSweep,
        state: Po3State,
        entry: Option<&EntryHit>,
        latest_ts: i64,
    ) -> PowerOf3 {
        PowerOf3 {
            id: sweep.id.clone(),
            symbol: self.symbol.clone(),
            tf: self.tf,
            direction: sweep.direction,
            state,
            context_kind: context_kind(&sweep.context),
            context_structure_ids: sweep.context.ids.clone(),
            context_timeframes: sweep.context.tfs.clone(),
            accumulation_start_ts: sweep.accumulation.start_ts,
            accumulation_end_ts: sweep.accumulation.end_ts,
            accumulation_high: sweep.accumulation.high,
            accumulation_low: sweep.accumulation.low,
            sweep_ts: sweep.sweep_ts,
            sweep_price: sweep.sweep_price,
            confirm_ts: entry.map(|e| e.ts).unwrap_or(sweep.sweep_ts),
            confirm_id: entry.map(|e| e.id.clone()).unwrap_or_default(),
            confirm_kind: entry.map(|e| e.kind).unwrap_or(ReversalConfirmKind::Cisd),
            entry_ts: entry.map(|e| e.ts),
            entry_price: entry.map(|e| e.price),
            entry_tf: entry.map(|e| e.tf),
            entry_kind: entry.map(|e| e.kind),
            entry_id: entry.map(|e| e.id.clone()),
            bos_id: None,
            quality_score: sweep.base_score,
            stage_boxes: build_candidate_stage_boxes(sweep, latest_ts),
        }
    }

    fn build_reversal_po3(
        &self,
        sweep: &PendingSweep,
        confirm: &ConfirmHit,
        entry: Option<&EntryHit>,
        score: u8,
        dist_start_ts: i64,
        bars_in_distribution: &[&Bar],
    ) -> PowerOf3 {
        let latest_ts = distribution_box_end(confirm.ts, self.tf);
        PowerOf3 {
            id: sweep.id.clone(),
            symbol: self.symbol.clone(),
            tf: self.tf,
            direction: sweep.direction,
            state: Po3State::ReversalConfirmed,
            context_kind: context_kind(&sweep.context),
            context_structure_ids: sweep.context.ids.clone(),
            context_timeframes: sweep.context.tfs.clone(),
            accumulation_start_ts: sweep.accumulation.start_ts,
            accumulation_end_ts: sweep.accumulation.end_ts,
            accumulation_high: sweep.accumulation.high,
            accumulation_low: sweep.accumulation.low,
            sweep_ts: sweep.sweep_ts,
            sweep_price: sweep.sweep_price,
            confirm_ts: confirm.ts,
            confirm_id: confirm.id.clone(),
            confirm_kind: confirm.kind,
            entry_ts: entry.map(|e| e.ts),
            entry_price: entry.map(|e| e.price),
            entry_tf: entry.map(|e| e.tf),
            entry_kind: entry.map(|e| e.kind),
            entry_id: entry.map(|e| e.id.clone()),
            bos_id: None,
            quality_score: score,
            stage_boxes: build_stage_boxes(
                sweep,
                confirm.ts,
                dist_start_ts,
                latest_ts,
                bars_in_distribution,
                None,
            ),
        }
    }

    fn emit_confirmations(
        &mut self,
        bar: &Bar,
        bars: &[Bar],
        structures: &[IctStructure],
        out: &mut Vec<StructureEvent>,
    ) {
        let mut remaining = Vec::new();
        let mut pending = std::mem::take(&mut self.pending);
        for mut sweep in pending.drain(..) {
            if bar.ts <= sweep.sweep_ts {
                remaining.push(sweep);
                continue;
            }
            sweep.bars_waited += 1;
            if sweep.bars_waited > self.cfg.max_bars_after_sweep {
                out.push(StructureEvent::Invalidated {
                    id: sweep.id.clone(),
                    kind: "power_of_3".to_string(),
                });
                self.emitted.remove(&sweep.id);
                continue;
            }
            let entry = lower_tf_entry(
                structures,
                &self.symbol,
                self.tf,
                sweep.direction,
                sweep.sweep_ts,
                bar.ts,
                sweep.sweep_price,
            );
            if let Some(entry) = entry {
                let should_update = sweep
                    .entry
                    .as_ref()
                    .map(|current| entry.ts < current.ts)
                    .unwrap_or(true);
                if should_update {
                    sweep.entry = Some(entry);
                    sweep.state = Po3State::EarlyReversal;
                    let po3 = self.build_candidate_po3(
                        &sweep,
                        Po3State::EarlyReversal,
                        sweep.entry.as_ref(),
                        bar.ts,
                    );
                    self.emitted.insert(sweep.id.clone(), po3.clone());
                    out.push(StructureEvent::Update(IctStructure::PowerOf3(po3)));
                }
            }
            let confirm = latest_confirm(
                structures,
                &self.symbol,
                self.tf,
                sweep.direction,
                sweep.sweep_ts,
                bar.ts,
                self.cfg.allow_cisd,
                self.cfg.allow_mss,
            );
            if let Some(confirm) = confirm {
                let score = sweep.base_score + confirm_score(confirm.kind);
                if score < self.cfg.min_quality_score {
                    out.push(StructureEvent::Invalidated {
                        id: sweep.id.clone(),
                        kind: "power_of_3".to_string(),
                    });
                    self.emitted.remove(&sweep.id);
                    continue;
                }
                let id = sweep.id.clone();
                if self.emitted_range_keys.contains(&id) {
                    continue;
                }
                sweep.state = Po3State::ReversalConfirmed;
                let dist_start_ts = sweep.entry.as_ref().map(|e| e.ts).unwrap_or(sweep.sweep_ts);
                let dist_bars: Vec<&Bar> = bars
                    .iter()
                    .filter(|b| b.ts >= dist_start_ts && b.ts <= bar.ts)
                    .collect();
                let po3 = self.build_reversal_po3(
                    &sweep,
                    &confirm,
                    sweep.entry.as_ref(),
                    score,
                    dist_start_ts,
                    &dist_bars,
                );
                self.emitted.insert(id.clone(), po3.clone());
                out.push(StructureEvent::Update(IctStructure::PowerOf3(po3)));
                self.confirmed.push(PendingConfirmedPo3 {
                    sweep,
                    confirm,
                    score,
                    id,
                    bars_waited: 0,
                });
            } else {
                remaining.push(sweep);
            }
        }
        self.pending = remaining;
        self.emit_distribution_acceptance(bar, bars, structures, out);
        self.update_distribution_boxes(bar, out);
        self.emit_bos_updates(structures, out);
    }

    fn emit_distribution_acceptance(
        &mut self,
        bar: &Bar,
        bars: &[Bar],
        structures: &[IctStructure],
        out: &mut Vec<StructureEvent>,
    ) {
        let mut remaining = Vec::new();
        let mut confirmed = std::mem::take(&mut self.confirmed);
        for mut candidate in confirmed.drain(..) {
            if bar.ts <= candidate.confirm.ts {
                remaining.push(candidate);
                continue;
            }
            candidate.bars_waited += 1;
            if distribution_failed(&candidate.sweep, bar) {
                out.push(StructureEvent::Invalidated {
                    id: candidate.id.clone(),
                    kind: "power_of_3".to_string(),
                });
                self.emitted.remove(&candidate.id);
                continue;
            }
            if distribution_accepted(&candidate.sweep, bar) {
                if self.emitted_range_keys.contains(&candidate.id) {
                    continue;
                }
                let latest_ts = distribution_box_end(candidate.confirm.ts, self.tf);
                let entry = candidate.sweep.entry.as_ref().cloned().or_else(|| {
                    lower_tf_entry(
                        structures,
                        &self.symbol,
                        self.tf,
                        candidate.sweep.direction,
                        candidate.sweep.sweep_ts,
                        bar.ts,
                        candidate.sweep.sweep_price,
                    )
                });
                let dist_start_ts = entry
                    .as_ref()
                    .map(|e| e.ts)
                    .unwrap_or(candidate.sweep.sweep_ts);
                let dist_bars: Vec<&Bar> = bars
                    .iter()
                    .filter(|b| b.ts >= dist_start_ts && b.ts <= bar.ts)
                    .collect();
                let stage_boxes = build_stage_boxes(
                    &candidate.sweep,
                    candidate.confirm.ts,
                    dist_start_ts,
                    latest_ts,
                    &dist_bars,
                    None,
                );
                let po3 = PowerOf3 {
                    id: candidate.id.clone(),
                    symbol: self.symbol.clone(),
                    tf: self.tf,
                    direction: candidate.sweep.direction,
                    state: Po3State::DistributionConfirmed,
                    context_kind: context_kind(&candidate.sweep.context),
                    context_structure_ids: candidate.sweep.context.ids.clone(),
                    context_timeframes: candidate.sweep.context.tfs.clone(),
                    accumulation_start_ts: candidate.sweep.accumulation.start_ts,
                    accumulation_end_ts: candidate.sweep.accumulation.end_ts,
                    accumulation_high: candidate.sweep.accumulation.high,
                    accumulation_low: candidate.sweep.accumulation.low,
                    sweep_ts: candidate.sweep.sweep_ts,
                    sweep_price: candidate.sweep.sweep_price,
                    confirm_ts: candidate.confirm.ts,
                    confirm_id: candidate.confirm.id,
                    confirm_kind: candidate.confirm.kind,
                    entry_ts: entry.as_ref().map(|e| e.ts),
                    entry_price: entry.as_ref().map(|e| e.price),
                    entry_tf: entry.as_ref().map(|e| e.tf),
                    entry_kind: entry.as_ref().map(|e| e.kind),
                    entry_id: entry.as_ref().map(|e| e.id.clone()),
                    bos_id: None,
                    quality_score: candidate.score.saturating_add(1),
                    stage_boxes,
                };
                self.emitted_range_keys.insert(candidate.id.clone());
                self.emitted.insert(candidate.id.clone(), po3.clone());
                out.push(StructureEvent::Update(IctStructure::PowerOf3(po3)));
                continue;
            }
            if candidate.bars_waited < DISTRIBUTION_ACCEPTANCE_BARS {
                remaining.push(candidate);
            } else {
                out.push(StructureEvent::Invalidated {
                    id: candidate.id.clone(),
                    kind: "power_of_3".to_string(),
                });
                self.emitted.remove(&candidate.id);
            }
        }
        self.confirmed = remaining;
    }

    fn update_distribution_boxes(&mut self, bar: &Bar, out: &mut Vec<StructureEvent>) {
        let ids: Vec<String> = self.emitted.keys().cloned().collect();
        for id in ids {
            let Some(po3) = self.emitted.get_mut(&id) else {
                continue;
            };
            if bar.ts < po3.confirm_ts {
                continue;
            }
            update_distribution_box(po3, bar);
            if bar.ts <= distribution_box_end(po3.confirm_ts, po3.tf) {
                out.push(StructureEvent::Update(IctStructure::PowerOf3(po3.clone())));
            }
        }
    }

    fn emit_bos_updates(&mut self, structures: &[IctStructure], out: &mut Vec<StructureEvent>) {
        let bos: Vec<&Bos> = structures
            .iter()
            .filter_map(|s| match s {
                IctStructure::Bos(b) if b.symbol == self.symbol && b.tf == self.tf => Some(b),
                _ => None,
            })
            .collect();
        for po3 in self.emitted.values_mut() {
            if po3.state == Po3State::DistributionConfirmed {
                continue;
            }
            if let Some(b) = bos
                .iter()
                .filter(|b| b.direction == po3.direction && b.break_ts >= po3.confirm_ts)
                .max_by_key(|b| b.break_ts)
            {
                po3.state = Po3State::DistributionConfirmed;
                po3.bos_id = Some(b.id.clone());
                po3.quality_score = po3.quality_score.saturating_add(1);
                if let Some(dist) = po3
                    .stage_boxes
                    .iter_mut()
                    .find(|b| b.stage == Po3Stage::Distribution)
                {
                    dist.label = Some(distribution_label(po3.direction, true));
                }
                out.push(StructureEvent::Update(IctStructure::PowerOf3(po3.clone())));
            }
        }
    }
}

impl Detector for Po3Detector {
    fn name(&self) -> &'static str {
        "po3"
    }

    fn on_closed(&mut self, bars: &[Bar], ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        if !self.cfg.enabled || !execution_tf_enabled(self.tf) {
            return Vec::new();
        }
        let Some(bar) = bars.last() else {
            return Vec::new();
        };
        let now_ts = bar.ts;
        self.sync_existing(ctx.structures, now_ts);
        let mut out = Vec::new();
        self.emit_confirmations(bar, bars, ctx.structures, &mut out);
        if let Some(candidate) = self.best_accumulation(bars, ctx.structures) {
            self.detect_sweep(bar, bars, candidate, ctx, &mut out);
        }
        out
    }

    fn apply_param(&mut self, key: &str, value: &Value) -> bool {
        match key {
            "enabled" => apply_bool(&mut self.cfg.enabled, value),
            "enabled_1m" if self.tf == Timeframe::M1 => apply_bool(&mut self.cfg.enabled, value),
            "enabled_5m" if self.tf == Timeframe::M5 => apply_bool(&mut self.cfg.enabled, value),
            "enabled_15m" if self.tf == Timeframe::M15 => apply_bool(&mut self.cfg.enabled, value),
            "enabled_30m" if self.tf == Timeframe::M30 => apply_bool(&mut self.cfg.enabled, value),
            "enabled_1h" if self.tf == Timeframe::H1 => apply_bool(&mut self.cfg.enabled, value),
            "enabled_4h" if self.tf == Timeframe::H4 => apply_bool(&mut self.cfg.enabled, value),
            "enabled_1d" if self.tf == Timeframe::D1 => apply_bool(&mut self.cfg.enabled, value),
            "min_accumulation_bars" => value.as_u64().is_some_and(|v| {
                apply_usize(&mut self.cfg.min_accumulation_bars, v.max(2) as usize)
            }),
            "max_accumulation_bars" => value.as_u64().is_some_and(|v| {
                apply_usize(&mut self.cfg.max_accumulation_bars, v.max(2) as usize)
            }),
            "max_range_atr_mult" => value
                .as_f64()
                .is_some_and(|v| apply_f64(&mut self.cfg.max_range_atr_mult, v.max(0.1))),
            "require_liquidity_pool" => value
                .as_bool()
                .is_some_and(|v| apply_bool_value(&mut self.cfg.require_liquidity_pool, v)),
            "max_bars_after_sweep" => value.as_u64().is_some_and(|v| {
                apply_usize(&mut self.cfg.max_bars_after_sweep, v.max(1) as usize)
            }),
            "min_quality_score" => value
                .as_u64()
                .is_some_and(|v| apply_u8(&mut self.cfg.min_quality_score, v.min(10) as u8)),
            "allow_cisd" => apply_bool(&mut self.cfg.allow_cisd, value),
            "allow_mss" => apply_bool(&mut self.cfg.allow_mss, value),
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.confirmed.clear();
        self.emitted.clear();
        self.emitted_range_keys.clear();
    }
}

fn distribution_accepted(sweep: &PendingSweep, bar: &Bar) -> bool {
    match sweep.direction {
        Direction::Bullish => bar.close > sweep.accumulation.high,
        Direction::Bearish => bar.close < sweep.accumulation.low,
    }
}

fn distribution_failed(sweep: &PendingSweep, bar: &Bar) -> bool {
    match sweep.direction {
        Direction::Bullish => bar.low <= sweep.sweep_price,
        Direction::Bearish => bar.high >= sweep.sweep_price,
    }
}

fn apply_bool(target: &mut bool, value: &Value) -> bool {
    value.as_bool().is_some_and(|v| apply_bool_value(target, v))
}

fn apply_bool_value(target: &mut bool, value: bool) -> bool {
    if *target == value {
        return false;
    }
    *target = value;
    true
}

fn apply_usize(target: &mut usize, value: usize) -> bool {
    if *target == value {
        return false;
    }
    *target = value;
    true
}

fn apply_u8(target: &mut u8, value: u8) -> bool {
    if *target == value {
        return false;
    }
    *target = value;
    true
}

fn apply_f64(target: &mut f64, value: f64) -> bool {
    if (*target - value).abs() < f64::EPSILON {
        return false;
    }
    *target = value;
    true
}

#[derive(Clone, Debug)]
struct ConfirmHit {
    id: String,
    ts: i64,
    kind: ReversalConfirmKind,
}

fn latest_confirm(
    structures: &[IctStructure],
    symbol: &str,
    tf: Timeframe,
    direction: Direction,
    after_ts: i64,
    before_or_at_ts: i64,
    allow_cisd: bool,
    allow_mss: bool,
) -> Option<ConfirmHit> {
    structures
        .iter()
        .filter_map(|s| match s {
            IctStructure::Cisd(c)
                if allow_cisd
                    && c.symbol == symbol
                    && c.tf == tf
                    && c.direction == direction
                    && c.break_ts > after_ts
                    && c.break_ts <= before_or_at_ts =>
            {
                Some(ConfirmHit {
                    id: c.id.clone(),
                    ts: c.break_ts,
                    kind: ReversalConfirmKind::Cisd,
                })
            }
            IctStructure::Mss(m)
                if allow_mss
                    && m.symbol == symbol
                    && m.tf == tf
                    && m.direction == direction
                    && m.break_ts > after_ts
                    && m.break_ts <= before_or_at_ts =>
            {
                Some(ConfirmHit {
                    id: m.id.clone(),
                    ts: m.break_ts,
                    kind: ReversalConfirmKind::Mss,
                })
            }
            _ => None,
        })
        .max_by_key(|c| (c.ts, confirm_score(c.kind)))
}

fn lower_tf_entry(
    structures: &[IctStructure],
    symbol: &str,
    execution_tf: Timeframe,
    direction: Direction,
    after_ts: i64,
    before_or_at_ts: i64,
    sweep_price: f64,
) -> Option<EntryHit> {
    let lower_tfs = entry_tf_ladder(execution_tf);
    structures
        .iter()
        .filter_map(|s| match s {
            IctStructure::Cisd(c)
                if c.symbol == symbol
                    && lower_tfs.contains(&c.tf)
                    && c.direction == direction
                    && c.break_ts > after_ts
                    && c.break_ts <= before_or_at_ts
                    && entry_respects_sweep(direction, c.break_price, sweep_price) =>
            {
                Some(EntryHit {
                    id: c.id.clone(),
                    tf: c.tf,
                    ts: c.break_ts,
                    price: c.leg_origin_price,
                    kind: ReversalConfirmKind::Cisd,
                })
            }
            IctStructure::Mss(m)
                if m.symbol == symbol
                    && lower_tfs.contains(&m.tf)
                    && m.direction == direction
                    && m.break_ts > after_ts
                    && m.break_ts <= before_or_at_ts
                    && entry_respects_sweep(direction, m.break_price, sweep_price) =>
            {
                Some(EntryHit {
                    id: m.id.clone(),
                    tf: m.tf,
                    ts: m.break_ts,
                    price: m.swing_price,
                    kind: ReversalConfirmKind::Mss,
                })
            }
            _ => None,
        })
        .min_by_key(|e| (e.ts, entry_kind_rank(e.kind)))
}

fn entry_respects_sweep(direction: Direction, price: f64, sweep_price: f64) -> bool {
    match direction {
        Direction::Bullish => price > sweep_price,
        Direction::Bearish => price < sweep_price,
    }
}

fn entry_kind_rank(kind: ReversalConfirmKind) -> u8 {
    match kind {
        ReversalConfirmKind::Mss => 0,
        ReversalConfirmKind::Cisd => 1,
    }
}

fn score_base(candidate: &AccumulationCandidate, context: &ContextMatch) -> u8 {
    context
        .directional_score
        .saturating_add(context.pd_score)
        .saturating_add(context.neutral_score)
        .saturating_add(if candidate.has_eq { 2 } else { 0 })
        .saturating_add(if candidate.has_touches { 2 } else { 0 })
        .saturating_add(1) // base score for any valid accumulation candidate
}

fn confirm_score(kind: ReversalConfirmKind) -> u8 {
    match kind {
        ReversalConfirmKind::Cisd => 1,
        ReversalConfirmKind::Mss => 2,
    }
}

fn execution_tf_enabled(tf: Timeframe) -> bool {
    matches!(
        tf,
        Timeframe::M1
            | Timeframe::M5
            | Timeframe::M15
            | Timeframe::M30
            | Timeframe::H1
            | Timeframe::H4
            | Timeframe::D1
    )
}

fn htf_ladder(tf: Timeframe) -> &'static [Timeframe] {
    match tf {
        Timeframe::M1 => &[Timeframe::M5, Timeframe::M15, Timeframe::H1],
        Timeframe::M5 => &[Timeframe::M15, Timeframe::H1, Timeframe::H4],
        Timeframe::M15 | Timeframe::M30 => &[Timeframe::H1, Timeframe::H4, Timeframe::D1],
        Timeframe::H1 => &[Timeframe::H4, Timeframe::D1, Timeframe::W1],
        Timeframe::H4 => &[Timeframe::D1, Timeframe::W1],
        Timeframe::D1 => &[Timeframe::W1],
        _ => &[],
    }
}

fn entry_tf_ladder(tf: Timeframe) -> &'static [Timeframe] {
    match tf {
        Timeframe::M5 => &[Timeframe::M1],
        Timeframe::M15 => &[Timeframe::M5, Timeframe::M1],
        Timeframe::M30 => &[Timeframe::M15, Timeframe::M5],
        Timeframe::H1 => &[Timeframe::M15, Timeframe::M5],
        Timeframe::H4 => &[Timeframe::H1, Timeframe::M15],
        Timeframe::D1 => &[Timeframe::H4, Timeframe::H1],
        _ => &[],
    }
}

fn context_match(
    symbol: &str,
    tf: Timeframe,
    direction: Direction,
    low: f64,
    high: f64,
    at_ts: i64,
    structures: &[IctStructure],
) -> ContextMatch {
    let ladder = htf_ladder(tf);
    let mut ctx = ContextMatch::default();
    for s in structures {
        if s.symbol() != symbol || !ladder.contains(&s.tf()) {
            continue;
        }
        let mut add = |kind: Po3ContextKind,
                       score: u8,
                       directional_gate: bool,
                       id: String,
                       stf: Timeframe| {
            if !ctx.ids.contains(&id) {
                ctx.ids.push(id);
            }
            if !ctx.tfs.contains(&stf) {
                ctx.tfs.push(stf);
            }
            if directional_gate {
                ctx.directional_score = ctx.directional_score.max(score);
                ctx.has_directional_gate = true;
            } else if matches!(kind, Po3ContextKind::HtfPremiumDiscount) {
                ctx.pd_score = ctx.pd_score.max(score);
                ctx.has_directional_gate = true;
            } else {
                ctx.neutral_score = ctx.neutral_score.max(score);
            }
            ctx.primary = Some(match ctx.primary {
                None => kind,
                Some(existing) if existing == kind => existing,
                Some(_) => Po3ContextKind::MixedHtfContext,
            });
        };
        match s {
            IctStructure::Fvg(f)
                if structure_available_at(s, at_ts)
                    && fvg_context_direction(f) == Some(direction)
                    && fvg_usable(f.state)
                    && overlap(low, high, f.price_low, f.price_high) =>
            {
                add(Po3ContextKind::HtfFvg, 2, true, f.id.clone(), f.tf)
            }
            IctStructure::OrderBlock(o)
                if structure_available_at(s, at_ts)
                    && o.direction == direction
                    && ob_usable(o.state)
                    && overlap(low, high, o.price_low, o.price_high) =>
            {
                add(Po3ContextKind::HtfOrderBlock, 2, true, o.id.clone(), o.tf)
            }
            IctStructure::BreakerBlock(b)
                if structure_available_at(s, at_ts)
                    && b.direction == direction
                    && zone_usable(b.state)
                    && overlap(low, high, b.price_low, b.price_high) =>
            {
                add(Po3ContextKind::HtfBreaker, 2, true, b.id.clone(), b.tf)
            }
            IctStructure::Ote(o)
                if structure_available_at(s, at_ts)
                    && o.direction == direction
                    && overlap(low, high, o.price_low, o.price_high) =>
            {
                add(Po3ContextKind::HtfOte, 2, true, o.id.clone(), o.tf)
            }
            IctStructure::PremiumDiscount(pd)
                if structure_available_at(s, at_ts)
                    && pd_direction_ok(pd.current_side, direction)
                    && overlap(low, high, pd.low, pd.high) =>
            {
                add(
                    Po3ContextKind::HtfPremiumDiscount,
                    1,
                    false,
                    pd.id.clone(),
                    pd.tf,
                )
            }
            IctStructure::Nwog(g) | IctStructure::Ndog(g)
                if structure_available_at(s, at_ts)
                    && gap_usable(g.state)
                    && overlap(low, high, g.price_low, g.price_high) =>
            {
                add(Po3ContextKind::GenericRange, 1, false, g.id.clone(), g.tf)
            }
            IctStructure::SessionRange(r)
                if structure_available_at(s, at_ts)
                    && r.session == SessionKind::Asia
                    && r.finalized
                    && overlap(low, high, r.low, r.high) =>
            {
                add(Po3ContextKind::SessionAsia, 1, false, r.id.clone(), r.tf)
            }
            _ => {}
        }
    }
    ctx.tfs.sort();
    ctx
}

fn fvg_usable(state: FvgState) -> bool {
    matches!(state, FvgState::Active | FvgState::Mitigated50)
}

fn fvg_context_direction(fvg: &Fvg) -> Option<Direction> {
    match fvg.state {
        FvgState::Active | FvgState::Mitigated50 => Some(fvg.direction),
        FvgState::Filled | FvgState::InvertedActive | FvgState::InvertedMitigated => None,
    }
}

fn structure_available_at(s: &IctStructure, at_ts: i64) -> bool {
    match s {
        IctStructure::Fvg(f) => f.ts_confirm <= at_ts,
        IctStructure::OrderBlock(o) => o.ts_confirm <= at_ts,
        IctStructure::BreakerBlock(b) => b.ts_confirm <= at_ts,
        IctStructure::Ote(o) => o.leg_end_ts <= at_ts,
        IctStructure::PremiumDiscount(pd) => pd.range_end_ts <= at_ts,
        IctStructure::Nwog(g) | IctStructure::Ndog(g) => g.new_open_ts <= at_ts,
        IctStructure::SessionRange(r) => r.ts_end <= at_ts,
        _ => true,
    }
}

fn ob_usable(state: ObState) -> bool {
    matches!(state, ObState::Active | ObState::Tested)
}

fn zone_usable(state: ZoneState) -> bool {
    matches!(state, ZoneState::Active | ZoneState::Tested)
}

fn gap_usable(state: GapState) -> bool {
    matches!(state, GapState::Active | GapState::Mitigated50)
}

fn pd_direction_ok(side: PdSide, direction: Direction) -> bool {
    matches!(
        (side, direction),
        (PdSide::Discount, Direction::Bullish) | (PdSide::Premium, Direction::Bearish)
    )
}

fn context_kind(ctx: &ContextMatch) -> Po3ContextKind {
    ctx.primary.unwrap_or(Po3ContextKind::GenericRange)
}

fn has_eqh_eql_overlap(
    structures: &[IctStructure],
    symbol: &str,
    tf: Timeframe,
    start_ts: i64,
    end_ts: i64,
    low: f64,
    high: f64,
) -> bool {
    structures.iter().any(|s| match s {
        IctStructure::EqualHighsLows(e) => {
            e.symbol == symbol
                && e.tf == tf
                && e.ts_end >= start_ts
                && e.ts_start <= end_ts
                && e.price >= low
                && e.price <= high
        }
        _ => false,
    })
}

fn boundary_touches(window: &[Bar], low: f64, high: f64, tolerance: f64) -> bool {
    let high_touches = window
        .iter()
        .filter(|b| (b.high - high).abs() <= tolerance)
        .count();
    let low_touches = window
        .iter()
        .filter(|b| (b.low - low).abs() <= tolerance)
        .count();
    high_touches >= 2 || low_touches >= 2
}

fn touch_tolerance(symbol: &str, atr: f64) -> f64 {
    (0.1 * atr).min(10.0 * pip_size(symbol))
}

fn overlap(a_low: f64, a_high: f64, b_low: f64, b_high: f64) -> bool {
    a_high >= b_low && b_high >= a_low
}

fn po3_candidate_id(
    symbol: &str,
    tf: Timeframe,
    direction: Direction,
    acc: &AccumulationCandidate,
    sweep_ts: i64,
) -> String {
    structure_id(&[
        symbol,
        tf.tag(),
        "po3",
        direction_tag(direction),
        &acc.start_ts.to_string(),
        &sweep_ts.to_string(),
    ])
}

fn direction_tag(direction: Direction) -> &'static str {
    match direction {
        Direction::Bullish => "bullish",
        Direction::Bearish => "bearish",
    }
}

fn accumulation_key(symbol: &str, tf: Timeframe, acc: &AccumulationCandidate) -> String {
    format!(
        "{}|{}|{}|{}|{:.5}|{:.5}",
        symbol,
        tf.tag(),
        acc.start_ts,
        acc.end_ts,
        acc.high,
        acc.low
    )
}

fn build_stage_boxes(
    sweep: &PendingSweep,
    confirm_ts: i64,
    dist_start_ts: i64,
    latest_ts: i64,
    bars_in_distribution: &[&Bar],
    existing_distribution: Option<&Po3StageBox>,
) -> Vec<Po3StageBox> {
    let acc = &sweep.accumulation;
    let manipulation = match sweep.direction {
        Direction::Bullish => Po3StageBox {
            stage: Po3Stage::Manipulation,
            ts_start: acc.end_ts,
            ts_end: sweep.sweep_ts,
            price_low: acc.low.min(sweep.sweep_price),
            price_high: acc.high,
            label: Some("PO3 Manip".into()),
        },
        Direction::Bearish => Po3StageBox {
            stage: Po3Stage::Manipulation,
            ts_start: acc.end_ts,
            ts_end: sweep.sweep_ts,
            price_low: acc.low,
            price_high: acc.high.max(sweep.sweep_price),
            label: Some("PO3 Manip".into()),
        },
    };
    let mut dist_low = existing_distribution
        .map(|b| b.price_low)
        .unwrap_or(f64::INFINITY);
    let mut dist_high = existing_distribution
        .map(|b| b.price_high)
        .unwrap_or(f64::NEG_INFINITY);
    for b in bars_in_distribution {
        dist_low = dist_low.min(b.low);
        dist_high = dist_high.max(b.high);
    }
    if !dist_low.is_finite() || !dist_high.is_finite() {
        // No bars after confirm found (rare edge case). Use the accumulation
        // range as a reasonable approximation — NOT the sweep price, which is
        // an extreme that makes the distribution box far too tall.
        dist_low = acc.low;
        dist_high = acc.high;
    }
    // Anchor the non-directional side to the FIRST distribution bar (the start
    // of the move, on the K-line) so opposite-direction wicks don't inflate the
    // box and it stays attached to the candles. Capping at the cross-TF entry
    // price instead can detach the box from the chart.
    if let Some(first) = bars_in_distribution.first() {
        match sweep.direction {
            Direction::Bearish => {
                if dist_high > first.high {
                    dist_high = first.high;
                }
            }
            Direction::Bullish => {
                if dist_low < first.low {
                    dist_low = first.low;
                }
            }
        }
    }
    vec![
        Po3StageBox {
            stage: Po3Stage::Accumulation,
            ts_start: acc.start_ts,
            ts_end: acc.end_ts,
            price_low: acc.low,
            price_high: acc.high,
            label: Some("PO3 Acc".into()),
        },
        manipulation,
        Po3StageBox {
            stage: Po3Stage::Distribution,
            ts_start: dist_start_ts,
            ts_end: latest_ts.max(confirm_ts),
            price_low: dist_low,
            price_high: dist_high,
            label: Some(distribution_label(sweep.direction, false)),
        },
    ]
}

fn build_candidate_stage_boxes(sweep: &PendingSweep, latest_ts: i64) -> Vec<Po3StageBox> {
    let acc = &sweep.accumulation;
    let manipulation_end = latest_ts.max(sweep.sweep_ts);
    let manipulation = match sweep.direction {
        Direction::Bullish => Po3StageBox {
            stage: Po3Stage::Manipulation,
            ts_start: acc.end_ts,
            ts_end: manipulation_end,
            price_low: acc.low.min(sweep.sweep_price),
            price_high: acc.high,
            label: Some("PO3? Manip".into()),
        },
        Direction::Bearish => Po3StageBox {
            stage: Po3Stage::Manipulation,
            ts_start: acc.end_ts,
            ts_end: manipulation_end,
            price_low: acc.low,
            price_high: acc.high.max(sweep.sweep_price),
            label: Some("PO3? Manip".into()),
        },
    };
    vec![
        Po3StageBox {
            stage: Po3Stage::Accumulation,
            ts_start: acc.start_ts,
            ts_end: acc.end_ts,
            price_low: acc.low,
            price_high: acc.high,
            label: Some("PO3? Acc".into()),
        },
        manipulation,
    ]
}

fn update_distribution_box(po3: &mut PowerOf3, bar: &Bar) {
    if po3.state < Po3State::ReversalConfirmed {
        return;
    }
    let max_end = distribution_box_end(po3.confirm_ts, po3.tf);
    if bar.ts > max_end {
        return;
    }
    let direction = po3.direction;
    if let Some(dist) = po3
        .stage_boxes
        .iter_mut()
        .find(|b| b.stage == Po3Stage::Distribution)
    {
        dist.ts_end = dist.ts_end.max(bar.ts).min(max_end);
        match direction {
            Direction::Bearish => {
                // distribution moves down: track the low. The high is anchored
                // to the distribution start at build time and is NOT extended,
                // so upward wicks don't inflate the box and it stays on the
                // candles.
                dist.price_low = dist.price_low.min(bar.low);
            }
            Direction::Bullish => {
                dist.price_high = dist.price_high.max(bar.high);
            }
        }
    }
}

fn distribution_box_end(confirm_ts: i64, tf: Timeframe) -> i64 {
    confirm_ts.saturating_add(tf.duration_ms().saturating_mul(DISTRIBUTION_BOX_BARS))
}

fn distribution_label(direction: Direction, confirmed: bool) -> String {
    let arrow = if direction == Direction::Bullish {
        "↑"
    } else {
        "↓"
    };
    if confirmed {
        format!("PO3✓ Dist{arrow}")
    } else {
        format!("PO3 Dist{arrow}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::types::{Cisd, Fvg, FvgState, Mss, ObState, OrderBlock};

    fn bar(tf: Timeframe, ts: i64, high: f64, low: f64, close: f64) -> Bar {
        Bar {
            symbol: "EURUSD".into(),
            tf,
            ts,
            open: close,
            high,
            low,
            close,
            volume: 1.0,
        }
    }

    fn run(
        det: &mut Po3Detector,
        history: &mut Vec<Bar>,
        b: Bar,
        structures: &[IctStructure],
    ) -> Vec<StructureEvent> {
        history.push(b);
        det.on_closed(
            history,
            &DetectorCtx {
                swings: &crate::detector::swing::SwingSeries::new(2),
                structures,
            },
        )
    }

    fn bullish_fvg(tf: Timeframe, low: f64, high: f64) -> IctStructure {
        IctStructure::Fvg(Fvg {
            id: "fvg-1".into(),
            symbol: "EURUSD".into(),
            tf,
            direction: Direction::Bullish,
            ts_open: 0,
            ts_confirm: 0,
            price_low: low,
            price_high: high,
            state: FvgState::Active,
            ts_filled: None,
            consumed_exit_ts: None,
        })
    }

    fn bullish_fvg_at(tf: Timeframe, low: f64, high: f64, ts_confirm: i64) -> IctStructure {
        IctStructure::Fvg(Fvg {
            id: format!("fvg-{ts_confirm}"),
            symbol: "EURUSD".into(),
            tf,
            direction: Direction::Bullish,
            ts_open: ts_confirm - tf.duration_ms(),
            ts_confirm,
            price_low: low,
            price_high: high,
            state: FvgState::Active,
            ts_filled: None,
            consumed_exit_ts: None,
        })
    }

    fn inverted_bullish_fvg(tf: Timeframe, low: f64, high: f64) -> IctStructure {
        IctStructure::Fvg(Fvg {
            id: "ifvg-1".into(),
            symbol: "EURUSD".into(),
            tf,
            direction: Direction::Bullish,
            ts_open: 0,
            ts_confirm: 0,
            price_low: low,
            price_high: high,
            state: FvgState::InvertedActive,
            ts_filled: None,
            consumed_exit_ts: None,
        })
    }

    fn bearish_ob(tf: Timeframe, low: f64, high: f64) -> IctStructure {
        IctStructure::OrderBlock(OrderBlock {
            id: "ob-1".into(),
            symbol: "EURUSD".into(),
            tf,
            direction: Direction::Bearish,
            ts_open: 0,
            ts_confirm: 0,
            price_low: low,
            price_high: high,
            state: ObState::Active,
        })
    }

    fn cisd(tf: Timeframe, direction: Direction, ts: i64) -> IctStructure {
        IctStructure::Cisd(Cisd {
            id: format!("cisd-{ts}"),
            symbol: "EURUSD".into(),
            tf,
            direction,
            leg_origin_ts: ts - tf.duration_ms(),
            leg_origin_price: 1.0,
            break_ts: ts,
            break_price: 1.0,
        })
    }

    fn mss(tf: Timeframe, direction: Direction, ts: i64) -> IctStructure {
        IctStructure::Mss(Mss {
            id: format!("mss-{ts}"),
            symbol: "EURUSD".into(),
            tf,
            direction,
            break_ts: ts,
            break_price: 1.0,
            swing_ts: ts - tf.duration_ms(),
            swing_price: 1.0,
        })
    }

    fn cisd_with_price(tf: Timeframe, direction: Direction, ts: i64, price: f64) -> IctStructure {
        IctStructure::Cisd(Cisd {
            id: format!("cisd-{tf:?}-{ts}"),
            symbol: "EURUSD".into(),
            tf,
            direction,
            leg_origin_ts: ts - tf.duration_ms(),
            leg_origin_price: price,
            break_ts: ts,
            break_price: price,
        })
    }

    #[test]
    fn contextual_bullish_po3_emits_with_stage_boxes() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 4,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.0990, 1.1015)];
        for i in 0..20 {
            let ts = i * tf.duration_ms();
            run(
                &mut det,
                &mut history,
                bar(tf, ts, 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        let sweep_ts = 20 * tf.duration_ms();
        run(
            &mut det,
            &mut history,
            bar(tf, sweep_ts, 1.1008, 1.0994, 1.1002),
            &structures,
        );
        let confirm_ts = 21 * tf.duration_ms();
        structures.push(cisd(tf, Direction::Bullish, confirm_ts));
        run(
            &mut det,
            &mut history,
            bar(tf, confirm_ts, 1.1020, 1.1003, 1.1018),
            &structures,
        );
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 22 * tf.duration_ms(), 1.1024, 1.1017, 1.1021),
            &structures,
        );
        let po3 = evs
            .into_iter()
            .find_map(|ev| match ev {
                StructureEvent::Update(IctStructure::PowerOf3(p)) => Some(p),
                _ => None,
            })
            .expect("po3");
        assert_eq!(po3.direction, Direction::Bullish);
        assert_eq!(po3.stage_boxes.len(), 3);
        assert_eq!(po3.stage_boxes[0].stage, Po3Stage::Accumulation);
        let distribution = po3
            .stage_boxes
            .iter()
            .find(|b| b.stage == Po3Stage::Distribution)
            .expect("distribution box");
        assert_eq!(distribution.ts_start, sweep_ts);
        assert_eq!(distribution.ts_end, distribution_box_end(confirm_ts, tf));
    }

    #[test]
    fn po3_records_lower_tf_entry_signal() {
        let tf = Timeframe::M15;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 4,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.0990, 1.1020)];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        let sweep_ts = 20 * tf.duration_ms();
        run(
            &mut det,
            &mut history,
            bar(tf, sweep_ts, 1.1008, 1.0994, 1.1002),
            &structures,
        );
        let entry_ts = sweep_ts + Timeframe::M5.duration_ms();
        structures.push(cisd_with_price(
            Timeframe::M5,
            Direction::Bullish,
            entry_ts,
            1.1009,
        ));
        let confirm_ts = 21 * tf.duration_ms();
        structures.push(cisd(tf, Direction::Bullish, confirm_ts));
        run(
            &mut det,
            &mut history,
            bar(tf, confirm_ts, 1.1020, 1.1003, 1.1018),
            &structures,
        );
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 22 * tf.duration_ms(), 1.1024, 1.1017, 1.1021),
            &structures,
        );
        let po3 = evs
            .into_iter()
            .find_map(|ev| match ev {
                StructureEvent::Update(IctStructure::PowerOf3(p)) => Some(p),
                _ => None,
            })
            .expect("po3");
        assert_eq!(po3.entry_tf, Some(Timeframe::M5));
        assert_eq!(po3.entry_ts, Some(entry_ts));
        assert_eq!(po3.entry_kind, Some(ReversalConfirmKind::Cisd));
    }

    #[test]
    fn no_htf_context_does_not_emit() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 3,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = Vec::new();
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        run(
            &mut det,
            &mut history,
            bar(tf, 20 * tf.duration_ms(), 1.1008, 1.0994, 1.1002),
            &structures,
        );
        structures.push(cisd(tf, Direction::Bullish, 21 * tf.duration_ms()));
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 21 * tf.duration_ms(), 1.1020, 1.1003, 1.1018),
            &structures,
        );
        assert!(!evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
    }

    #[test]
    fn future_htf_context_does_not_gate_po3() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 3,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let sweep_ts = 20 * tf.duration_ms();
        let mut structures = vec![bullish_fvg_at(
            Timeframe::H1,
            1.0990,
            1.1020,
            sweep_ts + Timeframe::H1.duration_ms(),
        )];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        run(
            &mut det,
            &mut history,
            bar(tf, sweep_ts, 1.1008, 1.0994, 1.1002),
            &structures,
        );
        structures.push(cisd(tf, Direction::Bullish, 21 * tf.duration_ms()));
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 21 * tf.duration_ms(), 1.1020, 1.1003, 1.1018),
            &structures,
        );
        assert!(!evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
    }

    #[test]
    fn inverted_fvg_is_not_used_as_direction_gate_without_inversion_ts() {
        let tf = Timeframe::M5;
        let mut bullish_det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 3,
                ..Default::default()
            },
        );
        let mut bullish_history = Vec::new();
        let mut structures = vec![inverted_bullish_fvg(Timeframe::H1, 1.0990, 1.1020)];
        for i in 0..20 {
            run(
                &mut bullish_det,
                &mut bullish_history,
                bar(tf, i * tf.duration_ms(), 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        run(
            &mut bullish_det,
            &mut bullish_history,
            bar(tf, 20 * tf.duration_ms(), 1.1008, 1.0994, 1.1002),
            &structures,
        );
        structures.push(cisd(tf, Direction::Bullish, 21 * tf.duration_ms()));
        let bullish_evs = run(
            &mut bullish_det,
            &mut bullish_history,
            bar(tf, 21 * tf.duration_ms(), 1.1020, 1.1003, 1.1018),
            &structures,
        );
        assert!(!bullish_evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
    }

    #[test]
    fn bearish_po3_can_use_mss_and_ob_context() {
        let tf = Timeframe::M15;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 4,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bearish_ob(Timeframe::H4, 1.1000, 1.1015)];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        let sweep_ts = 20 * tf.duration_ms();
        run(
            &mut det,
            &mut history,
            bar(tf, sweep_ts, 1.1018, 1.1002, 1.1008),
            &structures,
        );
        let confirm_ts = 21 * tf.duration_ms();
        structures.push(mss(tf, Direction::Bearish, confirm_ts));
        run(
            &mut det,
            &mut history,
            bar(tf, confirm_ts, 1.1009, 1.0988, 1.0990),
            &structures,
        );
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 22 * tf.duration_ms(), 1.0992, 1.0985, 1.0988),
            &structures,
        );
        assert!(evs.iter().any(|ev| matches!(ev, StructureEvent::Update(IctStructure::PowerOf3(p)) if p.direction == Direction::Bearish && p.stage_boxes.len() == 3)));
    }

    #[test]
    fn bullish_po3_fails_when_price_breaks_sweep_before_distribution_acceptance() {
        let tf = Timeframe::M15;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 4,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.1360, 1.1382)];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1380, 1.1369, 1.1373),
                &structures,
            );
        }
        let sweep_ts = 20 * tf.duration_ms();
        run(
            &mut det,
            &mut history,
            bar(tf, sweep_ts, 1.1374, 1.1367, 1.1372),
            &structures,
        );
        let confirm_ts = 21 * tf.duration_ms();
        structures.push(cisd(tf, Direction::Bullish, confirm_ts));
        run(
            &mut det,
            &mut history,
            bar(tf, confirm_ts, 1.1374, 1.1370, 1.1373),
            &structures,
        );
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 22 * tf.duration_ms(), 1.1373, 1.1366, 1.1368),
            &structures,
        );
        assert!(!evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
    }

    #[test]
    fn bullish_po3_requires_acceptance_above_accumulation_high() {
        let tf = Timeframe::M15;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 4,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.1360, 1.1382)];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.13806, 1.13686, 1.13730),
                &structures,
            );
        }
        let sweep_ts = 20 * tf.duration_ms();
        run(
            &mut det,
            &mut history,
            bar(tf, sweep_ts, 1.13724, 1.13676, 1.13723),
            &structures,
        );
        let confirm_ts = 21 * tf.duration_ms();
        structures.push(cisd(tf, Direction::Bullish, confirm_ts));
        run(
            &mut det,
            &mut history,
            bar(tf, confirm_ts, 1.13742, 1.13722, 1.13732),
            &structures,
        );
        for i in 22..=27 {
            let evs = run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.13736, 1.13688, 1.13700),
                &structures,
            );
            assert!(!evs
                .iter()
                .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
        }
    }

    #[test]
    fn wide_range_does_not_form_accumulation() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                max_range_atr_mult: 0.2,
                min_quality_score: 3,
                require_liquidity_pool: false,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.0950, 1.1100)];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1100, 1.0950, 1.1000),
                &structures,
            );
        }
        run(
            &mut det,
            &mut history,
            bar(tf, 20 * tf.duration_ms(), 1.1010, 1.0940, 1.0960),
            &structures,
        );
        structures.push(cisd(tf, Direction::Bullish, 21 * tf.duration_ms()));
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 21 * tf.duration_ms(), 1.1030, 1.0960, 1.1020),
            &structures,
        );
        assert!(!evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
    }

    #[test]
    fn liquidity_pool_requirement_blocks_flat_range_without_touches() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 3,
                require_liquidity_pool: true,
                max_range_atr_mult: 10.0,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.0990, 1.1020)];
        for i in 0..20 {
            let high = 1.1005 + (i as f64) * 0.00012;
            let low = 1.1000 + (i as f64) * 0.00012;
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), high, low, (high + low) / 2.0),
                &structures,
            );
        }
        run(
            &mut det,
            &mut history,
            bar(tf, 20 * tf.duration_ms(), 1.1006, 1.0994, 1.1001),
            &structures,
        );
        structures.push(cisd(tf, Direction::Bullish, 21 * tf.duration_ms()));
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 21 * tf.duration_ms(), 1.1020, 1.1003, 1.1018),
            &structures,
        );
        assert!(!evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
    }

    #[test]
    fn wick_break_without_close_back_inside_does_not_emit() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 3,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.0990, 1.1015)];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        run(
            &mut det,
            &mut history,
            bar(tf, 20 * tf.duration_ms(), 1.1008, 1.0994, 1.0997),
            &structures,
        );
        structures.push(cisd(tf, Direction::Bullish, 21 * tf.duration_ms()));
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 21 * tf.duration_ms(), 1.1020, 1.1003, 1.1018),
            &structures,
        );
        assert!(!evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
    }

    #[test]
    fn late_confirm_does_not_emit() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                max_bars_after_sweep: 1,
                min_quality_score: 3,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.0990, 1.1015)];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        run(
            &mut det,
            &mut history,
            bar(tf, 20 * tf.duration_ms(), 1.1008, 1.0994, 1.1002),
            &structures,
        );
        run(
            &mut det,
            &mut history,
            bar(tf, 21 * tf.duration_ms(), 1.1009, 1.1001, 1.1004),
            &structures,
        );
        structures.push(cisd(tf, Direction::Bullish, 22 * tf.duration_ms()));
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 22 * tf.duration_ms(), 1.1020, 1.1003, 1.1018),
            &structures,
        );
        assert!(!evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
    }

    #[test]
    fn score_below_threshold_does_not_emit() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 9,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.0990, 1.1015)];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        run(
            &mut det,
            &mut history,
            bar(tf, 20 * tf.duration_ms(), 1.1008, 1.0994, 1.1002),
            &structures,
        );
        structures.push(cisd(tf, Direction::Bullish, 21 * tf.duration_ms()));
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 21 * tf.duration_ms(), 1.1020, 1.1003, 1.1018),
            &structures,
        );
        assert!(!evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::PowerOf3(_)))));
    }

    #[test]
    fn bos_updates_distribution_confirmed() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 4,
                ..Default::default()
            },
        );
        let mut history = Vec::new();
        let mut structures = vec![bullish_fvg(Timeframe::H1, 1.0990, 1.1015)];
        for i in 0..20 {
            run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1010, 1.1000, 1.1005),
                &structures,
            );
        }
        run(
            &mut det,
            &mut history,
            bar(tf, 20 * tf.duration_ms(), 1.1008, 1.0994, 1.1002),
            &structures,
        );
        let confirm_ts = 21 * tf.duration_ms();
        structures.push(cisd(tf, Direction::Bullish, confirm_ts));
        run(
            &mut det,
            &mut history,
            bar(tf, confirm_ts, 1.1020, 1.1003, 1.1018),
            &structures,
        );
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 22 * tf.duration_ms(), 1.1024, 1.1017, 1.1021),
            &structures,
        );
        let po3_id = evs
            .into_iter()
            .find_map(|ev| match ev {
                StructureEvent::Update(IctStructure::PowerOf3(p)) => Some(p.id),
                _ => None,
            })
            .expect("po3");
        structures.push(IctStructure::Bos(Bos {
            id: "bos-1".into(),
            symbol: "EURUSD".into(),
            tf,
            direction: Direction::Bullish,
            break_ts: 23 * tf.duration_ms(),
            break_price: 1.1030,
            swing_ts: confirm_ts,
            swing_price: 1.1020,
        }));
        let evs = run(
            &mut det,
            &mut history,
            bar(tf, 23 * tf.duration_ms(), 1.1035, 1.1010, 1.1030),
            &structures,
        );
        assert!(evs.iter().any(|ev| matches!(ev, StructureEvent::Update(IctStructure::PowerOf3(p)) if p.id == po3_id && p.state == Po3State::DistributionConfirmed)));
    }

    #[test]
    fn distribution_box_does_not_extend_indefinitely() {
        let tf = Timeframe::M1;
        let mut po3 = PowerOf3 {
            id: "po3".into(),
            symbol: "EURUSD".into(),
            tf,
            direction: Direction::Bullish,
            state: Po3State::ReversalConfirmed,
            context_kind: Po3ContextKind::HtfFvg,
            context_structure_ids: vec![],
            context_timeframes: vec![Timeframe::M5],
            accumulation_start_ts: 0,
            accumulation_end_ts: tf.duration_ms(),
            accumulation_high: 1.1,
            accumulation_low: 1.0,
            sweep_ts: 2 * tf.duration_ms(),
            sweep_price: 0.99,
            confirm_ts: 3 * tf.duration_ms(),
            confirm_id: "cisd".into(),
            confirm_kind: ReversalConfirmKind::Cisd,
            entry_ts: None,
            entry_price: None,
            entry_tf: None,
            entry_kind: None,
            entry_id: None,
            bos_id: None,
            quality_score: 4,
            stage_boxes: vec![Po3StageBox {
                stage: Po3Stage::Distribution,
                ts_start: 3 * tf.duration_ms(),
                ts_end: distribution_box_end(3 * tf.duration_ms(), tf),
                price_low: 1.0,
                price_high: 1.1,
                label: None,
            }],
        };
        update_distribution_box(&mut po3, &bar(tf, 20 * tf.duration_ms(), 2.0, 0.5, 1.5));
        let dist = po3
            .stage_boxes
            .iter()
            .find(|b| b.stage == Po3Stage::Distribution)
            .expect("distribution box");
        assert_eq!(dist.ts_end, distribution_box_end(po3.confirm_ts, tf));
        assert_eq!(dist.price_low, 1.0);
        assert_eq!(dist.price_high, 1.1);
    }

    #[test]
    fn distribution_box_bearish_high_not_extended_by_wick() {
        // Opposite-direction (upward) wicks during a bearish distribution must
        // NOT inflate the box high; only the low should track the down move.
        let tf = Timeframe::M5;
        let entry = 1.1450;
        let mut po3 = PowerOf3 {
            id: "po3".into(),
            symbol: "EURUSD".into(),
            tf,
            direction: Direction::Bearish,
            state: Po3State::ReversalConfirmed,
            context_kind: Po3ContextKind::HtfFvg,
            context_structure_ids: vec![],
            context_timeframes: vec![Timeframe::M15],
            accumulation_start_ts: 0,
            accumulation_end_ts: tf.duration_ms(),
            accumulation_high: 1.1455,
            accumulation_low: 1.1445,
            sweep_ts: 2 * tf.duration_ms(),
            sweep_price: 1.1458,
            confirm_ts: 3 * tf.duration_ms(),
            confirm_id: "mss".into(),
            confirm_kind: ReversalConfirmKind::Mss,
            entry_ts: Some(3 * tf.duration_ms()),
            entry_price: Some(entry),
            entry_tf: Some(tf),
            entry_kind: Some(ReversalConfirmKind::Mss),
            entry_id: Some("e".into()),
            bos_id: None,
            quality_score: 10,
            stage_boxes: vec![Po3StageBox {
                stage: Po3Stage::Distribution,
                ts_start: 3 * tf.duration_ms(),
                ts_end: distribution_box_end(3 * tf.duration_ms(), tf),
                price_low: entry,
                price_high: entry,
                label: None,
            }],
        };
        // In-window bar: high wicks above entry, low pushes down.
        update_distribution_box(
            &mut po3,
            &bar(tf, 4 * tf.duration_ms(), 1.1462, 1.1420, 1.1425),
        );
        let dist = po3
            .stage_boxes
            .iter()
            .find(|b| b.stage == Po3Stage::Distribution)
            .expect("distribution box");
        assert_eq!(
            dist.price_high, entry,
            "bearish box high not extended by upward wick"
        );
        assert_eq!(dist.price_low, 1.1420, "bearish box low tracks down move");
    }

    #[test]
    fn distribution_box_bullish_low_not_extended_by_wick() {
        // Opposite-direction (downward) wicks during a bullish distribution must
        // NOT inflate the box low; only the high should track the up move.
        let tf = Timeframe::M5;
        let entry = 1.1000;
        let mut po3 = PowerOf3 {
            id: "po3".into(),
            symbol: "EURUSD".into(),
            tf,
            direction: Direction::Bullish,
            state: Po3State::ReversalConfirmed,
            context_kind: Po3ContextKind::HtfFvg,
            context_structure_ids: vec![],
            context_timeframes: vec![Timeframe::M15],
            accumulation_start_ts: 0,
            accumulation_end_ts: tf.duration_ms(),
            accumulation_high: 1.1010,
            accumulation_low: 1.0990,
            sweep_ts: 2 * tf.duration_ms(),
            sweep_price: 1.0985,
            confirm_ts: 3 * tf.duration_ms(),
            confirm_id: "cisd".into(),
            confirm_kind: ReversalConfirmKind::Cisd,
            entry_ts: Some(3 * tf.duration_ms()),
            entry_price: Some(entry),
            entry_tf: Some(tf),
            entry_kind: Some(ReversalConfirmKind::Cisd),
            entry_id: Some("e".into()),
            bos_id: None,
            quality_score: 10,
            stage_boxes: vec![Po3StageBox {
                stage: Po3Stage::Distribution,
                ts_start: 3 * tf.duration_ms(),
                ts_end: distribution_box_end(3 * tf.duration_ms(), tf),
                price_low: entry,
                price_high: entry,
                label: None,
            }],
        };
        // In-window bar: low wicks below entry, high pushes up.
        update_distribution_box(
            &mut po3,
            &bar(tf, 4 * tf.duration_ms(), 1.1040, 1.0980, 1.1035),
        );
        let dist = po3
            .stage_boxes
            .iter()
            .find(|b| b.stage == Po3Stage::Distribution)
            .expect("distribution box");
        assert_eq!(
            dist.price_low, entry,
            "bullish box low not extended by downward wick"
        );
        assert_eq!(dist.price_high, 1.1040, "bullish box high tracks up move");
    }

    #[test]
    fn distribution_confirmed_survives_rehydrate_during_seed() {
        // A hydrated DistributionConfirmed PO3 must NOT be invalidated when the
        // detector processes seed bars far past the distribution window.
        // Previously rehydrate_po3 pushed it into self.confirmed, and
        // emit_distribution_acceptance invalidated it after
        // DISTRIBUTION_ACCEPTANCE_BARS because the acceptance condition did
        // not re-match on the distant seed bars. This was the root cause of
        // PO3 disappearing on every restart.
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new("EURUSD", tf, Po3Config::default());
        let confirm_ts = 100 * tf.duration_ms();
        let po3 = PowerOf3 {
            id: "po3-dist".into(),
            symbol: "EURUSD".into(),
            tf,
            direction: Direction::Bullish,
            state: Po3State::DistributionConfirmed,
            context_kind: Po3ContextKind::HtfFvg,
            context_structure_ids: vec![],
            context_timeframes: vec![Timeframe::H1],
            accumulation_start_ts: 0,
            accumulation_end_ts: 50 * tf.duration_ms(),
            accumulation_high: 1.1010,
            accumulation_low: 1.0990,
            sweep_ts: 60 * tf.duration_ms(),
            sweep_price: 1.0985,
            confirm_ts,
            confirm_id: "cisd".into(),
            confirm_kind: ReversalConfirmKind::Cisd,
            entry_ts: Some(confirm_ts),
            entry_price: Some(1.1000),
            entry_tf: Some(tf),
            entry_kind: Some(ReversalConfirmKind::Cisd),
            entry_id: Some("e".into()),
            bos_id: None,
            quality_score: 10,
            stage_boxes: vec![],
        };
        let structures = vec![IctStructure::PowerOf3(po3.clone())];
        let mut history = Vec::new();
        // Feed bars FAR past the distribution window (simulating cold-start
        // seed of recent bars). More than DISTRIBUTION_ACCEPTANCE_BARS (6).
        for i in 200..215 {
            let evs = run(
                &mut det,
                &mut history,
                bar(tf, i * tf.duration_ms(), 1.1100, 1.1090, 1.1095),
                &structures,
            );
            assert!(
                !evs.iter().any(|ev| matches!(
                    ev,
                    StructureEvent::Invalidated { id, .. } if id == "po3-dist"
                )),
                "DistributionConfirmed PO3 must not be invalidated during seed (bar {i})"
            );
        }
        assert!(det.emitted.contains_key("po3-dist"));
        assert!(det.emitted_range_keys.contains("po3-dist"));
    }

    #[test]
    fn apply_param_updates_confirm_and_liquidity_filters() {
        let tf = Timeframe::M5;
        let mut det = Po3Detector::new(
            "EURUSD",
            tf,
            Po3Config {
                min_quality_score: 3,
                require_liquidity_pool: true,
                ..Default::default()
            },
        );

        assert!(det.apply_param("require_liquidity_pool", &Value::Bool(false)));
        assert!(!det.cfg.require_liquidity_pool);
        assert!(det.apply_param("allow_cisd", &Value::Bool(false)));
        assert!(!det.cfg.allow_cisd);
        assert!(det.apply_param("allow_mss", &Value::Bool(false)));
        assert!(!det.cfg.allow_mss);
        assert!(!det.apply_param("unknown", &Value::Bool(true)));
    }
}
