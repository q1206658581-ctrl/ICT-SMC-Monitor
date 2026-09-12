use std::time::Instant;

use crate::types::Bar;

use crate::detector::breaker_block::{from_order_block, step_breaker_state};
use crate::detector::engine::state::{
    IctEngine, DETECTOR_HISTORY, MAX_BAR_GAP_MULTIPLIER, PER_BAR_BUDGET_MS,
};
use crate::detector::swing::SwingSeries;
use crate::detector::types::{
    structure_id, FvgState, GapState, IctStructure, LevelMarker, LiquidityPoolKind, LiquiditySide,
    LiquiditySweep, ObState, OteZone, StructureEvent, ZoneState,
};
use crate::detector::Detector;

/// Wrap an arbitrary closure call (typically a detector callback) in
/// `catch_unwind` so a panicking detector doesn't take down the whole
/// subscription loop. KICKOFF §3.4 explicitly asks for this isolation.
/// Uses `AssertUnwindSafe` because the closures we call mutate detector
/// state intentionally; on panic we log and continue with no events.
pub(super) fn run_isolated<F>(name: &str, f: F) -> Vec<StructureEvent>
where
    F: FnOnce() -> Vec<StructureEvent>,
{
    let cell = std::cell::RefCell::new(Some(f));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let f = cell.borrow_mut().take().expect("once");
        f()
    }));
    match result {
        Ok(v) => v,
        Err(e) => {
            let msg = if let Some(s) = e.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".into()
            };
            tracing::error!(detector = %name, panic = %msg, "detector panicked; isolated");
            Vec::new()
        }
    }
}

fn zone_bounds(s: &IctStructure) -> Option<(i64, f64, f64, bool)> {
    match s {
        IctStructure::Fvg(f) => Some((
            f.ts_confirm,
            f.price_low,
            f.price_high,
            matches!(
                f.state,
                FvgState::Active | FvgState::Mitigated50 | FvgState::InvertedActive
            ),
        )),
        IctStructure::OrderBlock(o) => Some((
            o.ts_confirm,
            o.price_low,
            o.price_high,
            matches!(o.state, ObState::Active | ObState::Tested),
        )),
        IctStructure::BreakerBlock(b) => Some((
            b.ts_confirm,
            b.price_low,
            b.price_high,
            matches!(b.state, ZoneState::Active | ZoneState::Tested),
        )),
        IctStructure::VolumeImbalance(v) => Some((
            v.ts_confirm,
            v.price_low,
            v.price_high,
            matches!(v.state, GapState::Active | GapState::Mitigated50),
        )),
        _ => None,
    }
}

fn ranges_overlap(a_low: f64, a_high: f64, b_low: f64, b_high: f64) -> bool {
    a_high >= b_low && b_high >= a_low
}

impl IctEngine {
    pub fn on_closed_bar(&mut self, bar: &Bar) {
        self.process_closed_bar(bar, false);
    }

    /// Cold-start seeding variant: skips the bar-gap continuity guard.
    /// SQLite-backed history naturally contains weekends / holidays —
    /// these are not feed anomalies, so we must not `clear()` the
    /// bucket history every time the replay crosses one. Without this
    /// distinction the 1m history ends up trimmed to the last
    /// contiguous segment (≈497 bars in the live observation), which
    /// breaks PDH/PDL flush coverage on `apply_detector_param`.
    pub fn seed_closed_bar(&mut self, bar: &Bar) {
        self.process_closed_bar(bar, true);
    }

    pub(super) fn process_closed_bar(&mut self, bar: &Bar, seeding: bool) {
        let started = Instant::now();
        let key = (bar.symbol.clone(), bar.tf);
        let toggles_snapshot = self.toggles.clone();
        let mut events: Vec<StructureEvent> = Vec::new();
        if self
            .streams
            .get(&key)
            .and_then(|bucket| bucket.history.back())
            .map(|prev| bar.ts <= prev.ts)
            .unwrap_or(false)
        {
            if seeding {
                tracing::warn!(
                    symbol = %bar.symbol, tf = %bar.tf.tag(),
                    bar_ts = bar.ts,
                    prev_ts = self.streams.get(&key)
                        .and_then(|b| b.history.back())
                        .map(|b| b.ts),
                    "SEED_SKIP: bar skipped during seeding (continuity guard)"
                );
            }
            return;
        }
        // (See note below: we no longer mass-invalidate on weekend gaps,
        // so the discontinuity_ids vector that used to live here is gone.)
        let needs_reset = if seeding {
            false
        } else {
            let tf_dur = bar.tf.duration_ms();
            let max_gap = tf_dur.saturating_mul(MAX_BAR_GAP_MULTIPLIER);
            let bucket = self.ensure(key.clone());
            bucket
                .history
                .back()
                .map(|prev| bar.ts.saturating_sub(prev.ts) > max_gap)
                .unwrap_or(false)
        };
        // NOTE: do NOT mass-invalidate the (symbol, tf) bucket on a
        // weekend / market-closure gap. The original v1 implementation
        // wiped every active structure here, which made the cold-start
        // seed produce zero structures on H4 / D1 (whose 500-bar window
        // straddles ≥1 weekend, so every legitimate pre-gap FVG/OB got
        // killed before the user saw the chart). The continuity guard
        // below still resets detector working state + history so we
        // don't synthesize phantom cross-gap FVGs (the bug that caused
        // the first 49h FVG report); the historical structures stay in
        // place and continue to age via the normal state machine on
        // future bars. `discontinuity_ids` therefore remains empty.
        let structures_snapshot = self.detector_structures_snapshot("", &bar.symbol, bar.tf);
        let po3_replay_done = self.po3_replay_done.contains(&bar.symbol);
        // Skip PO3 snapshot during seeding - PO3 detector is skipped
        // (see `if seeding && name == "po3" && !po3_replay_done` below)
        // so the snapshot would be wasted. This avoids cloning thousands
        // of cross-TF structures per bar during cold-start.
        let po3_structures_snapshot = if seeding && !po3_replay_done {
            Vec::new()
        } else {
            self.detector_structures_snapshot("po3", &bar.symbol, bar.tf)
        };
        {
            let bucket = self.ensure(key.clone());
            if needs_reset {
                tracing::warn!(
                    symbol = %bar.symbol, tf = %bar.tf.tag(),
                    last_ts = bucket.history.back().map(|b| b.ts).unwrap_or(0),
                    new_ts = bar.ts,
                    "engine: bar gap exceeds threshold, resetting per-tf state"
                );
                bucket.history.clear();
                let n = bucket.swings.fractal_n;
                bucket.swings = SwingSeries::new(n);
                for det in bucket.detectors.iter_mut() {
                    // PdhPdlDetector accumulates the current NY day's
                    // high/low until the next 17:00 rollover. A weekend
                    // gap reset (Mon 02:00–05:00 EDT, depending on
                    // broker) does NOT cross 17:00 — wiping its state
                    // would silently drop today's accumulation and
                    // suppress every PDH/PDL emit until the next
                    // rollover, which the user reported as "PDH/PDL
                    // never appears". Skip it.
                    if matches!(det.name(), "pdh_pdl" | "opening_gap" | "session" | "po3") {
                        continue;
                    }
                    det.reset();
                }
            }
            if bucket.history.len() == DETECTOR_HISTORY {
                bucket.history.pop_front();
            }
            bucket.history.push_back(bar.clone());

            let history_slice: Vec<Bar> = bucket.history.iter().cloned().collect();
            let _swing_delta = bucket.swings.on_closed_bar(&history_slice);

            // Snapshot swings then borrow detectors mutably.
            let swings_snapshot = bucket.swings.clone();
            for det in bucket.detectors.iter_mut() {
                if !toggles_snapshot.is_enabled(det.name()) {
                    continue;
                }
                let name = det.name();
                // During cold-start seeding (recent-N replay + TV backfill) po3
                // starts from an empty working set, so re-running it over a
                // truncated window spuriously invalidates hydrated PO3 whose
                // confirm/distribution extends past the window edge (observed:
                // ~1900 spurious `invalidated` events shrinking 15m PO3 from 6
                // to 3 on cold start). Skip po3 while seeding and trust the
                // SQLite-hydrated set; live bars (seeding=false) re-enable
                // normal detection going forward.
                if seeding && name == "po3" && !po3_replay_done {
                    continue;
                }
                let det_structures_snapshot: &[IctStructure] = if name == "po3" {
                    &po3_structures_snapshot
                } else {
                    &structures_snapshot
                };
                let det_mut: &mut dyn Detector = det.as_mut();
                let mut evs = run_isolated(name, || {
                    let ctx = crate::detector::DetectorCtx {
                        swings: &swings_snapshot,
                        structures: det_structures_snapshot,
                    };
                    det_mut.on_closed(&history_slice, &ctx)
                });
                events.append(&mut evs);
                // Detector-driven flush — currently used by PDH/PDL to
                // surface the running NY day's high/low so users see
                // levels between 17:00 rollovers.
                let mut flushed = run_isolated(name, || det_mut.flush(&history_slice));
                events.append(&mut flushed);
            }
        }

        events.append(&mut self.m4b_context_events(bar, &events));
        events.append(&mut self.detect_pdh_pdl_sweeps(bar));
        events.append(&mut self.liquidity_reversal_events(bar, &events));

        // Historical structures persist across weekend gaps; only the
        // detector's working state was reset above. Nothing to invalidate
        // here — just flush the regular detector output.
        for ev in events {
            self.apply_and_broadcast(ev);
        }
        let elapsed = started.elapsed();
        if elapsed.as_millis() > PER_BAR_BUDGET_MS {
            tracing::warn!(
                symbol = %bar.symbol, tf = %bar.tf.tag(),
                elapsed_ms = elapsed.as_millis() as u64,
                phase = "on_closed_bar",
                "engine bar processing over budget"
            );
        }
    }

    /// Push the still-rolling bar update. Detectors may advance state
    /// machines but **must not** create new structures here.
    pub fn on_open_bar(&mut self, bar: &Bar) {
        let started = Instant::now();
        let key = (bar.symbol.clone(), bar.tf);
        let toggles_snapshot = self.toggles.clone();
        let mut events: Vec<StructureEvent> = Vec::new();
        // Rolling-bar callbacks are implemented only by FVG and Order Block,
        // and neither consumes DetectorCtx::structures. Building the regular
        // and PO3 snapshots here used to clone tens of thousands of hydrated
        // historical structures for every quote tick (up to once/second for
        // each symbol), monopolising the engine mutex and making the chart
        // appear to stop updating. Keep the context contract intact with an
        // empty slice; closed-bar callbacks still receive their full required
        // snapshots below.
        let empty_structures: &[IctStructure] = &[];
        {
            let bucket = self.ensure(key);
            let history_slice: Vec<Bar> = bucket.history.iter().cloned().collect();
            let swings_snapshot = bucket.swings.clone();
            for det in bucket.detectors.iter_mut() {
                if !toggles_snapshot.is_enabled(det.name()) {
                    continue;
                }
                let name = det.name();
                let det_mut: &mut dyn Detector = det.as_mut();
                let mut evs = run_isolated(name, || {
                    let ctx = crate::detector::DetectorCtx {
                        swings: &swings_snapshot,
                        structures: empty_structures,
                    };
                    det_mut.on_open(bar, &history_slice, &ctx)
                });
                events.append(&mut evs);
            }
        }
        events.append(&mut self.m4b_context_events(bar, &events));
        for ev in events {
            self.apply_and_broadcast(ev);
        }
        let elapsed = started.elapsed();
        if elapsed.as_millis() > PER_BAR_BUDGET_MS {
            tracing::warn!(
                symbol = %bar.symbol, tf = %bar.tf.tag(),
                elapsed_ms = elapsed.as_millis() as u64,
                phase = "on_open_bar",
                "engine bar processing over budget"
            );
        }
    }

    pub(super) fn apply_and_broadcast(&self, ev: StructureEvent) {
        let seeding = self.seeding.load(std::sync::atomic::Ordering::Relaxed);
        match &ev {
            StructureEvent::New(s) | StructureEvent::Update(s) => {
                let id = s.id().to_string();
                let sym = s.symbol().to_string();
                let tf = s.tf();
                let new_key = (sym.clone(), tf);
                let mut needs_index_insert = true;
                // If updating an existing structure whose (symbol, tf)
                // changed, remove the stale index entry.
                {
                    let old = self.structures.get(&id);
                    if let Some(old) = old {
                        let old_sym = old.symbol().to_string();
                        let old_tf = old.tf();
                        if old_sym != sym || old_tf != tf {
                            if let Some(mut ids) = self.structures_index.get_mut(&(old_sym, old_tf))
                            {
                                ids.retain(|i| i != &id);
                            }
                        } else {
                            needs_index_insert = false;
                        }
                    }
                }
                self.structures.insert(id.clone(), s.clone());
                // The primary map lookup above is the O(1) dedup guard. A
                // Vec::contains scan here used to walk tens of thousands of
                // ids on every live structure update.
                if needs_index_insert {
                    self.structures_index
                        .entry(new_key)
                        .or_default()
                        .push(id.clone());
                }
                if seeding {
                    self.seed_dirty_structure_ids.insert(id);
                }
            }
            StructureEvent::Invalidated { id, .. } => {
                if let Some((_, structure)) = self.structures.remove(id) {
                    let sym = structure.symbol().to_string();
                    let tf = structure.tf();
                    if let Some(mut ids) = self.structures_index.get_mut(&(sym, tf)) {
                        ids.retain(|i| i != id);
                    }
                }
            }
        }
        // During seeding (cold-start replay, TV Historical batch, PO3
        // replay) skip the broadcast to avoid overflowing the channel
        // (capacity 64) with tens of thousands of historical events.
        // The front-end re-fetches via list_structures after seed_complete.
        if !seeding {
            let _ = self.event_tx.send(ev);
        }
    }

    pub(super) fn detector_structures_snapshot(
        &self,
        detector_name: &str,
        symbol: &str,
        tf: crate::types::Timeframe,
    ) -> Vec<IctStructure> {
        // Use the secondary index instead of scanning all structures.
        // The old approach iterated every structure in the engine
        // (25k+) on every bar, making cold-start seeding take 10+
        // minutes (EURUSD 5m alone took 141s for 500 bars).
        let mut result = Vec::new();
        if detector_name == "po3" {
            // PO3 cross-TF context: collect all structures for this
            // symbol across every TF.
            for entry in self.structures_index.iter() {
                if entry.key().0 != symbol {
                    continue;
                }
                for id in entry.value() {
                    if let Some(s) = self.structures.get(id) {
                        result.push(s.value().clone());
                    }
                }
            }
        } else {
            // Common case: only structures matching (symbol, tf).
            if let Some(ids) = self.structures_index.get(&(symbol.to_string(), tf)) {
                for id in ids.iter() {
                    if let Some(s) = self.structures.get(id) {
                        result.push(s.value().clone());
                    }
                }
            }
        }
        result
    }

    pub(super) fn m4b_context_events(
        &mut self,
        bar: &Bar,
        current_events: &[StructureEvent],
    ) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        out.append(&mut self.breaker_events(bar, current_events));
        out.append(&mut self.ote_confluence_events(current_events, &out));
        out
    }

    fn breaker_events(
        &mut self,
        bar: &Bar,
        current_events: &[StructureEvent],
    ) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        for ev in current_events {
            if let StructureEvent::Update(IctStructure::OrderBlock(ob)) = ev {
                if ob.state != ObState::Mitigated {
                    continue;
                }
                let Some(breaker) = from_order_block(ob, bar) else {
                    continue;
                };
                if !self.structures.contains_key(&breaker.id) {
                    out.push(StructureEvent::New(IctStructure::BreakerBlock(breaker)));
                }
            }
        }

        let active_breakers: Vec<_> = self
            .structures_index
            .get(&(bar.symbol.clone(), bar.tf))
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| match self.structures.get(id)?.value() {
                        IctStructure::BreakerBlock(b)
                            if matches!(b.state, ZoneState::Active | ZoneState::Tested) =>
                        {
                            Some(b.clone())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        for breaker in active_breakers {
            let next = step_breaker_state(breaker.clone(), bar);
            if next.state != breaker.state {
                out.push(StructureEvent::Update(IctStructure::BreakerBlock(next)));
            }
        }
        out
    }

    fn ote_confluence_events(
        &mut self,
        current_events: &[StructureEvent],
        derived_events: &[StructureEvent],
    ) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        for ev in current_events.iter().chain(derived_events.iter()) {
            if let StructureEvent::New(IctStructure::Ote(mut ote)) = ev.clone() {
                let ids = self.ote_confluence_ids(&ote, current_events, derived_events);
                if ids != ote.confluent_structure_ids {
                    ote.confluent_structure_ids = ids;
                    out.push(StructureEvent::Update(IctStructure::Ote(ote)));
                }
            }
        }
        out
    }

    fn ote_confluence_ids(
        &self,
        ote: &OteZone,
        current_events: &[StructureEvent],
        derived_events: &[StructureEvent],
    ) -> Vec<String> {
        let mut ids = ote.confluent_structure_ids.clone();
        for ev in current_events.iter().chain(derived_events.iter()) {
            if let StructureEvent::New(s) | StructureEvent::Update(s) = ev {
                if s.symbol() != ote.symbol || s.tf() != ote.tf || s.id() == ote.id {
                    continue;
                }
                let Some((_start_ts, price_low, price_high, active)) = zone_bounds(s) else {
                    continue;
                };
                if active
                    && ranges_overlap(ote.price_low, ote.price_high, price_low, price_high)
                    && !ids.iter().any(|id| id == s.id())
                {
                    ids.push(s.id().to_string());
                }
            }
        }
        ids.sort();
        ids
    }

    pub(super) fn detect_pdh_pdl_sweeps(&mut self, bar: &Bar) -> Vec<StructureEvent> {
        if !self.toggles.is_enabled("liquidity") {
            return Vec::new();
        }
        let mut out = Vec::new();
        let levels: Vec<LevelMarker> = {
            let mut result = Vec::new();
            for entry in self.structures_index.iter() {
                if entry.key().0 != bar.symbol {
                    continue;
                }
                for id in entry.value() {
                    if let Some(s) = self.structures.get(id) {
                        match s.value() {
                            IctStructure::Pdh(level) | IctStructure::Pdl(level) => {
                                result.push(level.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
            result
        };
        for level in levels {
            let is_pdh = level.label == "PDH";
            let swept = if is_pdh {
                bar.high > level.price && bar.close < level.price
            } else {
                bar.low < level.price && bar.close > level.price
            };
            if !swept {
                continue;
            }
            let level_key = format!(
                "{}|{}|{}|{}|{}",
                bar.symbol,
                level.id,
                level.price,
                if is_pdh { "pdh" } else { "pdl" },
                bar.tf.tag()
            );
            if !self.swept_level_keys.insert(level_key) {
                continue;
            }
            let pool_tag = if is_pdh { "pdh" } else { "pdl" };
            let id = structure_id(&[
                &bar.symbol,
                bar.tf.tag(),
                "liquidity_sweep",
                pool_tag,
                &level.id,
                &bar.ts.to_string(),
            ]);
            out.push(StructureEvent::New(IctStructure::LiquiditySweep(
                LiquiditySweep {
                    id,
                    symbol: bar.symbol.clone(),
                    tf: bar.tf,
                    side: if is_pdh {
                        LiquiditySide::BuySide
                    } else {
                        LiquiditySide::SellSide
                    },
                    pool_kind: if is_pdh {
                        LiquidityPoolKind::Pdh
                    } else {
                        LiquidityPoolKind::Pdl
                    },
                    sweep_ts: bar.ts,
                    sweep_price: if is_pdh { bar.high } else { bar.low },
                    level_ts: level.source_ts.unwrap_or(level.valid_from_ts),
                    level_price: level.price,
                    close_price: bar.close,
                },
            )));
        }
        out
    }
}
