use crate::types::{Bar, Timeframe};

use crate::detector::engine::state::IctEngine;
use crate::detector::liquidity_reversal::maybe_reversal;
use crate::detector::types::{
    structure_id, Cisd, IctStructure, LiquiditySweep, Mss, ReversalConfirmKind, StructureEvent,
};

impl IctEngine {
    pub(super) fn liquidity_reversal_events(
        &mut self,
        bar: &Bar,
        events: &[StructureEvent],
    ) -> Vec<StructureEvent> {
        if !self.toggles.is_enabled("liquidity_reversal") {
            return Vec::new();
        }
        let key = (bar.symbol.clone(), bar.tf);
        let bucket = self.recent_sweeps.entry(key.clone()).or_default();
        for ev in events {
            if let StructureEvent::New(IctStructure::LiquiditySweep(sweep)) = ev {
                bucket.push_back(sweep.clone());
            }
        }
        let max_age = bar
            .tf
            .duration_ms()
            .saturating_mul(self.reversal_cfg.max_bars_after_sweep as i64 + 1);
        while bucket
            .front()
            .map(|s| bar.ts.saturating_sub(s.sweep_ts) > max_age)
            .unwrap_or(false)
        {
            bucket.pop_front();
        }
        let sweeps: Vec<LiquiditySweep> = bucket.iter().cloned().collect();
        let mut out = Vec::new();
        for ev in events {
            let (confirm_id, confirm_kind, direction, confirm_ts, confirm_price) = match ev {
                StructureEvent::New(IctStructure::Cisd(c)) => (
                    &c.id,
                    ReversalConfirmKind::Cisd,
                    c.direction,
                    c.break_ts,
                    c.leg_origin_price,
                ),
                StructureEvent::New(IctStructure::Mss(m)) => (
                    &m.id,
                    ReversalConfirmKind::Mss,
                    m.direction,
                    m.break_ts,
                    m.swing_price,
                ),
                _ => continue,
            };
            for sweep in &sweeps {
                let rev_id = structure_id(&[
                    &bar.symbol,
                    bar.tf.tag(),
                    "liquidity_reversal",
                    &sweep.id,
                    confirm_id,
                ]);
                if self.emitted_reversals.contains(&rev_id) {
                    continue;
                }
                let Some(rev) = maybe_reversal(
                    &bar.symbol,
                    bar.tf,
                    sweep,
                    confirm_id,
                    confirm_kind,
                    direction,
                    confirm_ts,
                    confirm_price,
                    bar.tf.duration_ms(),
                    &self.reversal_cfg,
                ) else {
                    continue;
                };
                self.emitted_reversals.insert(rev_id);
                out.push(rev);
            }
        }
        out
    }

    /// Snapshot of structures matching (symbol, tf).
    /// Cross-TF kinds (`pdh`, `pdl`, `kill_zone`) are always returned for
    /// the symbol regardless of `tf` — they are computed once on the source
    /// TF (1m for PDH/PDL, scheduler for KillZone) and shown on every chart.

    pub(super) fn invalidate_liquidity_reversals(&mut self) -> Vec<StructureEvent> {
        self.emitted_reversals.clear();
        self.structures
            .iter()
            .filter_map(|kv| {
                let structure = kv.value();
                (structure.kind_tag() == "liquidity_reversal").then(|| {
                    StructureEvent::Invalidated {
                        id: structure.id().to_string(),
                        kind: "liquidity_reversal".to_string(),
                    }
                })
            })
            .collect()
    }

    pub(super) fn rebuild_liquidity_reversals(&mut self) -> Vec<StructureEvent> {
        let mut events = self.invalidate_liquidity_reversals();
        if !self.toggles.is_enabled("liquidity_reversal") {
            return events;
        }

        let structures: Vec<IctStructure> = self
            .structures
            .iter()
            .map(|kv| kv.value().clone())
            .collect();
        let mut sweeps: Vec<LiquiditySweep> = structures
            .iter()
            .filter_map(|structure| match structure {
                IctStructure::LiquiditySweep(sweep) => Some(sweep.clone()),
                _ => None,
            })
            .collect();
        let mut confirms: Vec<(
            String,
            Timeframe,
            String,
            ReversalConfirmKind,
            crate::detector::types::Direction,
            i64,
            f64,
        )> = structures
            .iter()
            .filter_map(|structure| match structure {
                IctStructure::Cisd(Cisd {
                    symbol,
                    tf,
                    id,
                    direction,
                    break_ts,
                    leg_origin_price,
                    ..
                }) => Some((
                    symbol.clone(),
                    *tf,
                    id.clone(),
                    ReversalConfirmKind::Cisd,
                    *direction,
                    *break_ts,
                    *leg_origin_price,
                )),
                IctStructure::Mss(Mss {
                    symbol,
                    tf,
                    id,
                    direction,
                    break_ts,
                    swing_price,
                    ..
                }) => Some((
                    symbol.clone(),
                    *tf,
                    id.clone(),
                    ReversalConfirmKind::Mss,
                    *direction,
                    *break_ts,
                    *swing_price,
                )),
                _ => None,
            })
            .collect();
        sweeps.sort_by_key(|sweep| sweep.sweep_ts);
        confirms.sort_by_key(|(_, _, _, _, _, confirm_ts, _)| *confirm_ts);

        for (symbol, tf, confirm_id, confirm_kind, direction, confirm_ts, confirm_price) in confirms
        {
            for sweep in sweeps
                .iter()
                .filter(|sweep| sweep.symbol == symbol && sweep.tf == tf)
            {
                let rev_id = structure_id(&[
                    &symbol,
                    tf.tag(),
                    "liquidity_reversal",
                    &sweep.id,
                    &confirm_id,
                ]);
                if self.emitted_reversals.contains(&rev_id) {
                    continue;
                }
                let Some(ev) = maybe_reversal(
                    &symbol,
                    tf,
                    sweep,
                    &confirm_id,
                    confirm_kind,
                    direction,
                    confirm_ts,
                    confirm_price,
                    tf.duration_ms(),
                    &self.reversal_cfg,
                ) else {
                    continue;
                };
                self.emitted_reversals.insert(rev_id);
                events.push(ev);
            }
        }
        events
    }
}
