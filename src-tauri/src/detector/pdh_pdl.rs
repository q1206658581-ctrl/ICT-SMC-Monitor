//! PDH/PDL detector — §5.2.8.
//!
//! Consumes 1m closed bars only. Tracks the previous day's high/low using
//! one of two day-boundary modes (configurable per-detector at runtime via
//! `apply_param("daily_boundary", "ny_local"|"ny_1700")`):
//!
//! * `NyLocal` (default) — **NY local civil midnight** (00:00 America/New_York).
//!   Daylight-saving aware via chrono-tz. This is what most charting
//!   platforms call "PDH/PDL" by default.
//! * `Ny1700` — ICT-original 17:00 NY rollover, delegated to
//!   `Timeframe::D1.boundary_align` (no extra timezone code).
//!
//! When a new 1m bar closes whose ts crosses the active boundary, we
//! finalize the levels for the just-completed day and emit them.

use chrono::{DateTime, Datelike, TimeZone, Utc};
use chrono_tz::America::New_York;

use super::types::{structure_id, IctStructure, LevelMarker, StructureEvent};
use super::{Detector, DetectorCtx};
use crate::types::{Bar, Timeframe};

/// Which civil-day boundary to use when computing PDH/PDL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DailyBoundary {
    /// NY local 00:00, DST-aware (default per user decision 2026-06-16).
    NyLocal,
    /// ICT 17:00 NY rollover (legacy / keeps original `Timeframe::D1`
    /// behavior). Delegates to `Timeframe::D1.boundary_align` so we don't
    /// duplicate timezone code.
    Ny1700,
}

impl Default for DailyBoundary {
    fn default() -> Self {
        DailyBoundary::NyLocal
    }
}

impl DailyBoundary {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ny_local" => Some(DailyBoundary::NyLocal),
            "ny_1700" => Some(DailyBoundary::Ny1700),
            _ => None,
        }
    }
    pub fn tag(self) -> &'static str {
        match self {
            DailyBoundary::NyLocal => "ny_local",
            DailyBoundary::Ny1700 => "ny_1700",
        }
    }
    /// One nominal day in ms. Always 24h — DST shifts are absorbed by
    /// `align_day_open` returning the correct UTC instant for "next NY
    /// midnight" rather than naive +24h arithmetic.
    pub fn nominal_day_ms() -> i64 {
        24 * 60 * 60_000
    }
}

/// Resolve `ts_ms` to the UTC ms of the start of the civil NY day it
/// belongs to, under the chosen boundary mode. DST handled by chrono-tz.
fn align_day_open(ts_ms: i64, mode: DailyBoundary) -> i64 {
    match mode {
        DailyBoundary::Ny1700 => Timeframe::D1.boundary_align(ts_ms),
        DailyBoundary::NyLocal => {
            let utc: DateTime<Utc> = Utc.timestamp_millis_opt(ts_ms).single().expect("ts");
            let ny = utc.with_timezone(&New_York);
            let date = ny.date_naive();
            // Build NY-local midnight on that date and resolve back to UTC.
            // `single()` is fine: 00:00 never falls into DST forward-skip
            // (skip happens at 02:00 in spring), and the ambiguous fall-back
            // hour is 01:00–02:00 (also not 00:00). Use earliest() as a
            // belt-and-suspenders fallback in case a future tz rule changes.
            let local = New_York
                .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
                .single()
                .or_else(|| {
                    New_York
                        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
                        .earliest()
                })
                .expect("NY midnight resolvable");
            local.with_timezone(&Utc).timestamp_millis()
        }
    }
}

pub struct PdhPdlDetector {
    pub symbol: String,
    pub mode: DailyBoundary,
    /// Day-open ts currently being accumulated.
    current_day_open: Option<i64>,
    cur_high: f64,
    cur_low: f64,
    cur_high_ts: i64,
    cur_low_ts: i64,
    /// Last emitted level ids so we can invalidate on rollover.
    last_pdh_id: Option<String>,
    last_pdl_id: Option<String>,
    last_seen_ts: i64,
}

impl PdhPdlDetector {
    pub fn new(symbol: impl Into<String>) -> Self {
        Self::with_mode(symbol, DailyBoundary::default())
    }

    pub fn with_mode(symbol: impl Into<String>, mode: DailyBoundary) -> Self {
        Self {
            symbol: symbol.into(),
            mode,
            current_day_open: None,
            cur_high: f64::NEG_INFINITY,
            cur_low: f64::INFINITY,
            cur_high_ts: i64::MIN,
            cur_low_ts: i64::MIN,
            last_pdh_id: None,
            last_pdl_id: None,
            last_seen_ts: i64::MIN,
        }
    }

    /// Emit New PDH/PDL for the **currently accumulated** NY day even
    /// when no rollover has happened yet. This is what makes
    /// `replay_detector_with_history` produce levels — without it, replay
    /// only emits on the rare case the 1500-bar window straddles 17:00 NY,
    /// which leaves the chart blank for hours after a toggle-off→on cycle.
    ///
    /// Idempotent: re-emits the same id if state hasn't changed (engine
    /// dedupes via `apply_and_broadcast`'s structures map upsert).
    /// Compute the **previous civil day's** high/low directly from the
    /// 1m bar history slice the engine passes in, and emit New PDH/PDL.
    ///
    /// Why this exists: rollover (the `on_closed` Some(prev_open) arm)
    /// is the only path that sets PDH/PDL during normal live operation,
    /// but a) cold-start replay seldom straddles the boundary by more
    /// than one day, and b) a runtime mode switch (`apply_param`)
    /// resets the detector and re-replays history — neither path is
    /// guaranteed to cross the boundary, so without a fallback the
    /// chart would be blank. The previous fallback used
    /// `cur_high` / `cur_low`, but those reflect "since the latest
    /// bar's day_open" which can miss the early hours of the previous
    /// day when the history slice starts mid-day. That produced the
    /// "wrong PDH/PDL value" bug user reported on 2026-06-16.
    ///
    /// Strategy:
    /// 1. Take the latest bar in `history` to find "today's" day_open
    ///    (using the active mode).
    /// 2. The previous day window is `[prev_open, day_open)`, where
    ///    `prev_open = align_day_open(day_open - 1ms, mode)` so DST
    ///    boundaries fall out automatically.
    /// 3. If history's earliest 1m ts > prev_open, the slice does not
    ///    fully cover the previous day → don't emit anything (better
    ///    blank than wrong).
    /// 4. Otherwise scan the slice for high/low within the window and
    ///    emit New PDH/PDL with id keyed on prev_open so subsequent
    ///    rollover cleanly replaces it.
    ///
    /// Idempotent across consecutive flushes (same id ⇒ engine upserts).
    pub fn force_emit_prev_day(&mut self, history: &[Bar]) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        // Skip when an actual rollover has already produced PDH/PDL —
        // that pair is canonical, flush must not duplicate.
        if self.last_pdh_id.is_some() || self.last_pdl_id.is_some() {
            return out;
        }

        // Need at least one 1m bar to anchor "today".
        let last = match history.iter().rev().find(|b| b.tf == Timeframe::M1) {
            Some(b) => b,
            None => return out,
        };
        let day_open = align_day_open(last.ts, self.mode);
        // `prev_open` = boundary one civil day before `day_open`.
        // Subtracting 1ms then aligning lets chrono-tz absorb DST shifts.
        let prev_open = align_day_open(day_open - 1, self.mode);
        if prev_open >= day_open {
            return out;
        }

        // Coverage check: history's first 1m bar must be <= prev_open
        // for the window to be fully covered. Otherwise the high/low
        // we'd compute is biased to the late-day portion only.
        let first_1m_ts = match history.iter().find(|b| b.tf == Timeframe::M1) {
            Some(b) => b.ts,
            None => return out,
        };
        if first_1m_ts > prev_open {
            return out;
        }

        // Scan the window [prev_open, day_open) for high / low.
        let mut hi = f64::NEG_INFINITY;
        let mut lo = f64::INFINITY;
        let mut hi_ts = i64::MIN;
        let mut lo_ts = i64::MIN;
        let mut any = false;
        for b in history.iter().filter(|b| b.tf == Timeframe::M1) {
            if b.ts < prev_open {
                continue;
            }
            if b.ts >= day_open {
                break;
            }
            if b.high > hi {
                hi = b.high;
                hi_ts = b.ts;
            }
            if b.low < lo {
                lo = b.low;
                lo_ts = b.ts;
            }
            any = true;
        }
        if !any || !hi.is_finite() || !lo.is_finite() {
            return out;
        }

        let pdh_id = structure_id(&[&self.symbol, "1m", "pdh", &prev_open.to_string()]);
        let pdl_id = structure_id(&[&self.symbol, "1m", "pdl", &prev_open.to_string()]);
        tracing::info!(
            target: "pdh_pdl_trace",
            stage = "force_emit_prev_day",
            sym = %self.symbol,
            mode = ?self.mode,
            prev_open,
            day_open,
            history_len = history.len(),
            first_1m_ts,
            pdh_id = %pdh_id,
            pdl_id = %pdl_id,
            pdh_price = hi,
            pdl_price = lo,
            "flush emitting pdh/pdl from history slice"
        );
        out.push(StructureEvent::New(IctStructure::Pdh(LevelMarker {
            id: pdh_id,
            symbol: self.symbol.clone(),
            tf: Timeframe::M1,
            price: hi,
            label: "PDH".into(),
            confirmed_at_ts: Some(day_open),
            valid_from_ts: prev_open,
            valid_until_ts: day_open,
            source_ts: Some(hi_ts),
        })));
        out.push(StructureEvent::New(IctStructure::Pdl(LevelMarker {
            id: pdl_id,
            symbol: self.symbol.clone(),
            tf: Timeframe::M1,
            price: lo,
            label: "PDL".into(),
            confirmed_at_ts: Some(day_open),
            valid_from_ts: prev_open,
            valid_until_ts: day_open,
            source_ts: Some(lo_ts),
        })));
        // Do not set last_pdh_id/last_pdl_id — those track rollover
        // emits used to drive the rollover-time invalidate path. A
        // future real rollover into a *different* prev_open will emit
        // a new pair under a different id (different hash input);
        // the engine's structures map keeps both alive momentarily
        // until the rollover invalidate hits. Leaving rollover's
        // bookkeeping untouched keeps that path correct.
        out
    }
}

impl Detector for PdhPdlDetector {
    fn name(&self) -> &'static str {
        "pdh_pdl"
    }

    fn flush(&mut self, history: &[Bar]) -> Vec<StructureEvent> {
        self.force_emit_prev_day(history)
    }

    fn apply_param(&mut self, key: &str, value: &serde_json::Value) -> bool {
        match key {
            "daily_boundary" => {
                let s = match value.as_str() {
                    Some(s) => s,
                    None => return false,
                };
                match DailyBoundary::parse(s) {
                    Some(m) => {
                        if self.mode == m {
                            return false;
                        }
                        self.mode = m;
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.current_day_open = None;
        self.cur_high = f64::NEG_INFINITY;
        self.cur_low = f64::INFINITY;
        self.cur_high_ts = i64::MIN;
        self.cur_low_ts = i64::MIN;
        self.last_pdh_id = None;
        self.last_pdl_id = None;
        self.last_seen_ts = i64::MIN;
    }

    fn on_closed(&mut self, bars: &[Bar], _ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        let last = match bars.last() {
            Some(b) => b,
            None => return out,
        };
        if last.tf != Timeframe::M1 {
            return out;
        }
        if last.ts <= self.last_seen_ts {
            return out;
        }
        self.last_seen_ts = last.ts;

        let day_open = align_day_open(last.ts, self.mode);

        match self.current_day_open {
            None => {
                self.current_day_open = Some(day_open);
                self.cur_high = last.high;
                self.cur_low = last.low;
                self.cur_high_ts = last.ts;
                self.cur_low_ts = last.ts;
            }
            Some(prev_open) if prev_open == day_open => {
                if last.high > self.cur_high {
                    self.cur_high = last.high;
                    self.cur_high_ts = last.ts;
                }
                if last.low < self.cur_low {
                    self.cur_low = last.low;
                    self.cur_low_ts = last.ts;
                }
            }
            Some(prev_open) => {
                // Rolled into a new NY day. Finalize PDH/PDL for the completed
                // day (prev_open .. day_open) and emit fresh markers.
                let valid_from = day_open;
                // Nominal +24h is correct for valid-window bookkeeping under both
                // modes; a DST day is still rendered as a 24h slot on the chart, the
                // next rollover handles the real boundary via align_day_open.
                let next_day = day_open + DailyBoundary::nominal_day_ms();
                let pdh_id = structure_id(&[&self.symbol, "1m", "pdh", &prev_open.to_string()]);
                let pdl_id = structure_id(&[&self.symbol, "1m", "pdl", &prev_open.to_string()]);

                tracing::info!(
                    target: "pdh_pdl_trace",
                    stage = "on_closed:rollover",
                    sym = %self.symbol,
                    prev_open,
                    day_open,
                    new_pdh_id = %pdh_id,
                    new_pdl_id = %pdl_id,
                    prev_pdh_id = ?self.last_pdh_id,
                    prev_pdl_id = ?self.last_pdl_id,
                    pdh_price = self.cur_high,
                    pdl_price = self.cur_low,
                    "rollover firing pdh/pdl emit"
                );

                if let Some(prev_pdh) = self.last_pdh_id.take() {
                    out.push(StructureEvent::Invalidated {
                        id: prev_pdh,
                        kind: "pdh".into(),
                    });
                }
                if let Some(prev_pdl) = self.last_pdl_id.take() {
                    out.push(StructureEvent::Invalidated {
                        id: prev_pdl,
                        kind: "pdl".into(),
                    });
                }

                let pdh_price = self.cur_high;
                let pdl_price = self.cur_low;
                let pdh_source_ts = self.cur_high_ts;
                let pdl_source_ts = self.cur_low_ts;

                out.push(StructureEvent::New(IctStructure::Pdh(LevelMarker {
                    id: pdh_id.clone(),
                    symbol: self.symbol.clone(),
                    tf: Timeframe::M1,
                    price: pdh_price,
                    label: "PDH".into(),
                    confirmed_at_ts: Some(day_open),
                    valid_from_ts: valid_from,
                    valid_until_ts: next_day,
                    source_ts: Some(pdh_source_ts),
                })));
                out.push(StructureEvent::New(IctStructure::Pdl(LevelMarker {
                    id: pdl_id.clone(),
                    symbol: self.symbol.clone(),
                    tf: Timeframe::M1,
                    price: pdl_price,
                    label: "PDL".into(),
                    confirmed_at_ts: Some(day_open),
                    valid_from_ts: valid_from,
                    valid_until_ts: next_day,
                    source_ts: Some(pdl_source_ts),
                })));
                self.last_pdh_id = Some(pdh_id);
                self.last_pdl_id = Some(pdl_id);

                // Start the new day's accumulation with `last`.
                self.current_day_open = Some(day_open);
                self.cur_high = last.high;
                self.cur_low = last.low;
                self.cur_high_ts = last.ts;
                self.cur_low_ts = last.ts;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::swing::SwingSeries;
    use chrono::TimeZone;
    use chrono_tz::America::New_York;

    fn b1m(ts: i64, hi: f64, lo: f64) -> Bar {
        Bar {
            symbol: "X".into(),
            tf: Timeframe::M1,
            ts,
            open: (hi + lo) / 2.0,
            high: hi,
            low: lo,
            close: (hi + lo) / 2.0,
            volume: 0.0,
        }
    }
    fn run_with_mode(bars: &[Bar], mode: DailyBoundary) -> Vec<StructureEvent> {
        let mut s = SwingSeries::default();
        let mut d = PdhPdlDetector::with_mode("X", mode);
        let mut all = Vec::new();
        for end in 1..=bars.len() {
            let slice = &bars[..end];
            s.on_closed_bar(slice);
            let ctx = DetectorCtx::new(&s);
            all.extend(d.on_closed(slice, &ctx));
        }
        all
    }
    fn run(bars: &[Bar]) -> Vec<StructureEvent> {
        // Legacy helper used by the original 17:00-NY tests; switch to
        // Ny1700 mode explicitly so `ny_1700()` boundary fixtures still apply.
        run_with_mode(bars, DailyBoundary::Ny1700)
    }

    /// 17:00 NY in UTC ms for given Y/M/D.
    fn ny_1700(y: i32, m: u32, d: u32) -> i64 {
        New_York
            .with_ymd_and_hms(y, m, d, 17, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn pdh_pdl_emit_on_day_rollover() {
        let day_a = ny_1700(2024, 1, 8); // 2024-01-08 17:00 NY
        let day_b = ny_1700(2024, 1, 9); // next NY day
                                         // Day A: low 1.10, high 1.20.
        let bars = vec![
            b1m(day_a + 60_000, 1.20, 1.15),
            b1m(day_a + 120_000, 1.18, 1.10),
            b1m(day_a + 180_000, 1.17, 1.13),
            // Day B starts:
            b1m(day_b + 60_000, 1.30, 1.25),
        ];
        let evs = run(&bars);
        let pdh = evs
            .iter()
            .find(|e| matches!(e, StructureEvent::New(IctStructure::Pdh(_))));
        let pdl = evs
            .iter()
            .find(|e| matches!(e, StructureEvent::New(IctStructure::Pdl(_))));
        assert!(pdh.is_some(), "expected PDH, got {:?}", evs);
        assert!(pdl.is_some(), "expected PDL, got {:?}", evs);
        if let Some(StructureEvent::New(IctStructure::Pdh(m))) = pdh {
            assert!((m.price - 1.20).abs() < 1e-9);
        }
        if let Some(StructureEvent::New(IctStructure::Pdl(m))) = pdl {
            assert!((m.price - 1.10).abs() < 1e-9);
        }
    }

    #[test]
    fn no_emit_within_same_day() {
        let day = ny_1700(2024, 1, 8);
        let bars = vec![
            b1m(day + 60_000, 1.20, 1.10),
            b1m(day + 120_000, 1.22, 1.05),
        ];
        let evs = run(&bars);
        assert!(evs.is_empty());
    }

    /// Without a rollover, flush should still surface PDH/PDL by
    /// scanning the **previous** day window from the history slice.
    /// This is what keeps PDH/PDL visible after a runtime mode switch
    /// or a cold-start replay whose 1500-bar window doesn't happen to
    /// straddle the boundary.
    ///
    /// History fully covers the previous day → emit real high/low.
    #[test]
    fn flush_yields_prev_day_when_history_covers_window() {
        let day_a = ny_1700(2024, 1, 8); // start of "previous day"
        let day_b = ny_1700(2024, 1, 9); // start of "today"
                                         // History: covers all of day A + a bar into day B (today).
                                         // First bar exactly at prev_open so coverage check passes
                                         // (first_1m_ts == prev_open).
        let bars = vec![
            b1m(day_a, 1.20, 1.10),           // day A start
            b1m(day_a + 120_000, 1.22, 1.05), // day A high/low
            b1m(day_a + 180_000, 1.18, 1.08), // day A
            b1m(day_b + 60_000, 1.30, 1.25),  // today
        ];
        let mut d = PdhPdlDetector::with_mode("X", DailyBoundary::Ny1700);
        // Don't call on_closed (otherwise rollover fires) — just feed
        // history straight into flush like a fresh-replay-without-rollover
        // would.
        let flushed = <PdhPdlDetector as crate::detector::Detector>::flush(&mut d, &bars);
        let pdh = flushed
            .iter()
            .find(|e| matches!(e, StructureEvent::New(IctStructure::Pdh(_))));
        let pdl = flushed
            .iter()
            .find(|e| matches!(e, StructureEvent::New(IctStructure::Pdl(_))));
        assert!(pdh.is_some(), "expected PDH from flush, got {:?}", flushed);
        assert!(pdl.is_some(), "expected PDL from flush, got {:?}", flushed);
        if let Some(StructureEvent::New(IctStructure::Pdh(m))) = pdh {
            assert!(
                (m.price - 1.22).abs() < 1e-9,
                "PDH must be day-A high 1.22, got {}",
                m.price
            );
        }
        if let Some(StructureEvent::New(IctStructure::Pdl(m))) = pdl {
            assert!(
                (m.price - 1.05).abs() < 1e-9,
                "PDL must be day-A low 1.05, got {}",
                m.price
            );
        }
    }

    /// History's earliest 1m bar is **inside** the previous-day window
    /// (i.e. the slice doesn't cover the full window) → flush must
    /// refuse to emit, since the high/low it would compute is biased
    /// towards the late-day portion only and would be wrong. Better
    /// blank than wrong.
    #[test]
    fn flush_no_op_when_history_starts_mid_prev_day() {
        let day_a = ny_1700(2024, 1, 8);
        let day_b = ny_1700(2024, 1, 9);
        // History starts 6 hours into day A — earlier high/low missing.
        let bars = vec![
            b1m(day_a + 6 * 60 * 60_000, 1.18, 1.12),
            b1m(day_a + 12 * 60 * 60_000, 1.19, 1.14),
            b1m(day_b + 60_000, 1.30, 1.25),
        ];
        let mut d = PdhPdlDetector::with_mode("X", DailyBoundary::Ny1700);
        let flushed = <PdhPdlDetector as crate::detector::Detector>::flush(&mut d, &bars);
        assert!(
            flushed.is_empty(),
            "flush should be no-op when history doesn\'t cover prev-day, got {:?}",
            flushed
        );
    }

    /// Switching mode at runtime: the same history yields different
    /// PDH/PDL depending on the boundary mode. NyLocal's prev-day
    /// (00:00–24:00 NY) and Ny1700's prev-day (17:00–17:00 NY) cover
    /// different windows and so different highs/lows.
    #[test]
    fn flush_uses_active_mode_for_prev_day_window() {
        // Build bars across both 17:00 boundaries so each mode's
        // prev-day window resolves to different data.
        let local_day_a = ny_midnight(2024, 1, 8);
        let local_day_b = ny_midnight(2024, 1, 9);
        // First bar at local_day_a so NyLocal coverage check passes
        // (first_1m_ts == local_day_a == NyLocal prev_open).
        let bars = vec![
            b1m(local_day_a, 1.22, 1.10), // NY 00:00 = NyLocal day-A start
            b1m(local_day_a + 14 * 60 * 60_000, 1.18, 1.05), // NY 14:00 = before 17:00
            b1m(local_day_a + 18 * 60 * 60_000, 1.25, 1.15), // NY 18:00 = after 17:00 (Ny1700 current-day)
            b1m(local_day_b + 60_000, 1.30, 1.28),           // today (both modes)
        ];
        // NyLocal mode: prev-day = local_day_a 00:00..local_day_b 00:00.
        // Window includes bars 0..2 → high = max(1.22, 1.18, 1.25) = 1.25,
        // low = min(1.10, 1.05, 1.15) = 1.05.
        let mut d = PdhPdlDetector::with_mode("X", DailyBoundary::NyLocal);
        let evs = <PdhPdlDetector as crate::detector::Detector>::flush(&mut d, &bars);
        if let Some(StructureEvent::New(IctStructure::Pdh(m))) = evs
            .iter()
            .find(|e| matches!(e, StructureEvent::New(IctStructure::Pdh(_))))
        {
            assert!(
                (m.price - 1.25).abs() < 1e-9,
                "NyLocal PDH must be 1.25, got {}",
                m.price
            );
        } else {
            panic!("expected NyLocal PDH, got {:?}", evs);
        }
        // History does not cover the FULL Ny1700 prev-day window
        // (Ny1700 prev-day starts at NY 06 17:00 = 21:00 UTC 2024-01-06,
        // before our earliest bar). Coverage check should refuse to emit.
        let mut d2 = PdhPdlDetector::with_mode("X", DailyBoundary::Ny1700);
        let evs2 = <PdhPdlDetector as crate::detector::Detector>::flush(&mut d2, &bars);
        assert!(
            evs2.is_empty(),
            "Ny1700 flush must refuse when slice starts after prev-1700-day open, got {:?}",
            evs2
        );
    }

    /// NY local 00:00 in UTC ms for given Y/M/D.
    fn ny_midnight(y: i32, m: u32, d: u32) -> i64 {
        New_York
            .with_ymd_and_hms(y, m, d, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    /// `NyLocal` mode: rollover at NY 00:00 (default per option A).
    #[test]
    fn ny_local_rollover_at_midnight() {
        let day_a = ny_midnight(2024, 1, 8);
        let day_b = ny_midnight(2024, 1, 9);
        let bars = vec![
            b1m(day_a + 60_000, 1.20, 1.15),
            b1m(day_a + 120_000, 1.18, 1.10),
            // crosses NY midnight
            b1m(day_b + 60_000, 1.30, 1.25),
        ];
        let mut s = SwingSeries::default();
        let mut d = PdhPdlDetector::with_mode("X", DailyBoundary::NyLocal);
        let mut evs = Vec::new();
        for end in 1..=bars.len() {
            let slice = &bars[..end];
            s.on_closed_bar(slice);
            let ctx = DetectorCtx::new(&s);
            evs.extend(d.on_closed(slice, &ctx));
        }
        let pdh = evs
            .iter()
            .find(|e| matches!(e, StructureEvent::New(IctStructure::Pdh(_))));
        let pdl = evs
            .iter()
            .find(|e| matches!(e, StructureEvent::New(IctStructure::Pdl(_))));
        assert!(
            pdh.is_some(),
            "expected PDH at NY midnight rollover, got {:?}",
            evs
        );
        if let Some(StructureEvent::New(IctStructure::Pdh(m))) = pdh {
            assert!(
                (m.price - 1.20).abs() < 1e-9,
                "PDH should be day-A high 1.20, got {}",
                m.price
            );
        }
        if let Some(StructureEvent::New(IctStructure::Pdl(m))) = pdl {
            assert!(
                (m.price - 1.10).abs() < 1e-9,
                "PDL should be day-A low 1.10, got {}",
                m.price
            );
        }
    }

    /// `NyLocal` mode under DST: spring-forward (2024-03-10) and
    /// fall-back (2024-11-03). The boundary should still resolve to a
    /// real UTC instant on both transitions and a 24h-cadence rollover
    /// across the transition day must still produce exactly one PDH/PDL.
    #[test]
    fn ny_local_dst_spring_forward_and_fall_back() {
        // Spring-forward: 2024-03-10. NY midnight = 05:00 UTC on
        // 2024-03-10; next NY midnight = 04:00 UTC on 2024-03-11
        // (only 23h between them in UTC).
        let dst_day = ny_midnight(2024, 3, 10);
        let next_day = ny_midnight(2024, 3, 11);
        assert_eq!(
            next_day - dst_day,
            23 * 60 * 60_000,
            "spring-forward = 23h UTC"
        );
        let bars = vec![
            b1m(dst_day + 60_000, 1.20, 1.15),
            b1m(next_day + 60_000, 1.30, 1.25),
        ];
        let mut s = SwingSeries::default();
        let mut d = PdhPdlDetector::with_mode("X", DailyBoundary::NyLocal);
        let mut evs = Vec::new();
        for end in 1..=bars.len() {
            let slice = &bars[..end];
            s.on_closed_bar(slice);
            evs.extend(d.on_closed(slice, &DetectorCtx::new(&s)));
        }
        let pdh = evs
            .iter()
            .filter(|e| matches!(e, StructureEvent::New(IctStructure::Pdh(_))))
            .count();
        assert_eq!(
            pdh, 1,
            "spring-forward should still produce exactly one PDH, got {}",
            pdh
        );

        // Fall-back: 2024-11-03. NY midnight = 04:00 UTC on 2024-11-03;
        // next NY midnight = 05:00 UTC on 2024-11-04 (25h in UTC).
        let dst_day = ny_midnight(2024, 11, 3);
        let next_day = ny_midnight(2024, 11, 4);
        assert_eq!(next_day - dst_day, 25 * 60 * 60_000, "fall-back = 25h UTC");
    }

    /// `apply_param("daily_boundary", ...)` switches the active mode.
    #[test]
    fn apply_param_daily_boundary_switch() {
        // Default constructor → NyLocal per option A.
        let mut d = PdhPdlDetector::new("X");
        assert_eq!(d.mode, DailyBoundary::NyLocal);
        let ok = <PdhPdlDetector as crate::detector::Detector>::apply_param(
            &mut d,
            "daily_boundary",
            &serde_json::Value::String("ny_1700".into()),
        );
        assert!(ok);
        assert_eq!(d.mode, DailyBoundary::Ny1700);
        let back = <PdhPdlDetector as crate::detector::Detector>::apply_param(
            &mut d,
            "daily_boundary",
            &serde_json::Value::String("ny_local".into()),
        );
        assert!(back);
        assert_eq!(d.mode, DailyBoundary::NyLocal);
        let bad = <PdhPdlDetector as crate::detector::Detector>::apply_param(
            &mut d,
            "daily_boundary",
            &serde_json::Value::String("garbage".into()),
        );
        assert!(!bad);
        assert_eq!(
            d.mode,
            DailyBoundary::NyLocal,
            "bad value must not change mode"
        );
    }

    /// Once a 17:00 rollover has produced PDH/PDL, the running day's
    /// high/low must NOT be surfaced as a second pair — that would be
    /// CDH/CDL (current day high/low), a different ICT concept. Option A
    /// per user decision: PDH/PDL is the previous-NY-day pair only.
    #[test]
    fn flush_no_op_after_rollover() {
        let day_a = ny_1700(2024, 1, 8);
        let day_b = ny_1700(2024, 1, 9);
        let bars = vec![
            b1m(day_a + 60_000, 1.20, 1.10),  // day A
            b1m(day_b + 60_000, 1.30, 1.25),  // day B (rollover here)
            b1m(day_b + 120_000, 1.31, 1.24), // day B running
        ];
        let mut s = SwingSeries::default();
        let mut d = PdhPdlDetector::with_mode("X", DailyBoundary::Ny1700);
        for end in 1..=bars.len() {
            let slice = &bars[..end];
            s.on_closed_bar(slice);
            let ctx = DetectorCtx::new(&s);
            d.on_closed(slice, &ctx);
        }
        // Rollover already emitted PDH/PDL for day A; flush should now
        // be a no-op.
        // After rollover, last_pdh_id is set so flush is no-op even with history present.
        let flushed = <PdhPdlDetector as crate::detector::Detector>::flush(&mut d, &bars);
        assert!(
            flushed.is_empty(),
            "flush should be no-op after rollover, got {:?}",
            flushed
        );
    }
}
