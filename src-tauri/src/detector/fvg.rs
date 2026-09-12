//! FVG detector with full IFVG state-machine extension (§5.2.14).
//!
//! The detector consumes closed bars on its TF and emits `StructureEvent`s.
//! State transitions are checked on every newly-closed bar plus on every
//! still-rolling bar (`on_open`) so mitigation/inversion lights up live.

use super::types::{structure_id, Direction, Fvg, FvgState, IctStructure, StructureEvent};
use super::Detector;
use crate::aggregator::next_window_boundary;
use crate::types::{Bar, Timeframe};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct FvgConfig {
    /// Minimum gap height in pips. Gaps below this are dropped at creation.
    pub min_size_pips: f64,
    /// Pip size for the symbol (EURUSD = 0.0001).
    pub pip_size: f64,
}

impl Default for FvgConfig {
    fn default() -> Self {
        Self {
            min_size_pips: 0.0,
            pip_size: 0.0001,
        }
    }
}

pub struct FvgDetector {
    pub symbol: String,
    pub tf: Timeframe,
    pub cfg: FvgConfig,
    /// Tracked active gaps that haven't reached `InvertedMitigated` yet.
    tracked: Vec<Fvg>,
    /// Last closed-bar ts we've already scanned for new 3-bar formations.
    /// Used to make `on_closed` idempotent if it gets re-fed the same slice.
    last_seen_close_ts: i64,
    /// Last state change per gap. A rolling candle is cumulative and may be
    /// delivered many times; once it fills a gap it must not use the same
    /// candle again to manufacture a post-fill inversion.
    last_transition: HashMap<String, (i64, FvgState)>,
}

impl FvgDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: FvgConfig) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            tracked: Vec::new(),
            last_seen_close_ts: i64::MIN,
            last_transition: HashMap::new(),
        }
    }

    pub fn tracked(&self) -> &[Fvg] {
        &self.tracked
    }

    /// Hydrate from a previously persisted set (cold start). Caller passes
    /// only structures matching this detector's (symbol, tf).
    pub fn hydrate(&mut self, fvgs: Vec<Fvg>) {
        for f in fvgs {
            // skip terminal state
            if f.state != FvgState::InvertedMitigated {
                if let Some(ts_filled) = f.ts_filled {
                    self.last_transition
                        .insert(f.id.clone(), (ts_filled, FvgState::Filled));
                }
                self.tracked.push(f);
            }
        }
    }
}

fn fvg_id(symbol: &str, tf: Timeframe, ts_open: i64, dir: Direction) -> String {
    let dir_tag = match dir {
        Direction::Bullish => "bull",
        Direction::Bearish => "bear",
    };
    structure_id(&[symbol, tf.tag(), "fvg", &ts_open.to_string(), dir_tag])
}

/// Detect a 3-bar FVG anchored at the *last* bar in `bars` (i.e. bar[i]).
/// Returns Some(Fvg{Active}) if the gap exists and meets `min_size_pips`.
fn detect_three_bar(symbol: &str, tf: Timeframe, cfg: &FvgConfig, bars: &[Bar]) -> Option<Fvg> {
    if bars.len() < 3 {
        return None;
    }
    let i2 = &bars[bars.len() - 3];
    let i1 = &bars[bars.len() - 2];
    let i0 = &bars[bars.len() - 1];
    // H1/H4 FVGs can become DXY PDA context, so their three formation
    // candles must be consecutive canonical parent windows. Cold-start seed
    // deliberately preserves history across market/data gaps for PDH/PDL;
    // without this local guard, a missing weekday H4 window makes three
    // vector-adjacent rows look like a valid three-candle FVG (07/08 19:00,
    // 23:00 and 07/09 11:00 was one real example).
    if matches!(tf, Timeframe::H1 | Timeframe::H4)
        && (next_window_boundary(tf, i2.ts) != i1.ts || next_window_boundary(tf, i1.ts) != i0.ts)
    {
        return None;
    }
    let min = cfg.min_size_pips * cfg.pip_size;

    // Bullish: bar[i-2].high < bar[i].low
    if i2.high < i0.low {
        let lo = i2.high;
        let hi = i0.low;
        if hi - lo < min {
            return None;
        }
        return Some(Fvg {
            id: fvg_id(symbol, tf, i2.ts, Direction::Bullish),
            symbol: symbol.into(),
            tf,
            direction: Direction::Bullish,
            ts_open: i2.ts,
            ts_confirm: i0.ts,
            price_low: lo,
            price_high: hi,
            state: FvgState::Active,
            ts_filled: None,
            consumed_exit_ts: None,
        });
    }
    // Bearish: bar[i-2].low > bar[i].high
    if i2.low > i0.high {
        let lo = i0.high;
        let hi = i2.low;
        if hi - lo < min {
            return None;
        }
        return Some(Fvg {
            id: fvg_id(symbol, tf, i2.ts, Direction::Bearish),
            symbol: symbol.into(),
            tf,
            direction: Direction::Bearish,
            ts_open: i2.ts,
            ts_confirm: i0.ts,
            price_low: lo,
            price_high: hi,
            state: FvgState::Active,
            ts_filled: None,
            consumed_exit_ts: None,
        });
    }
    None
}

/// Advance one tracked Fvg's state machine using a candle's [low, high].
/// Returns `Some(new_state)` if it changed; emits at most one transition
/// per call (deterministic & avoids skipping intermediate Update events).
fn step_state(f: &Fvg, low: f64, high: f64) -> Option<FvgState> {
    let mid = (f.price_low + f.price_high) / 2.0;

    match f.state {
        FvgState::Active => {
            let fully_filled = match f.direction {
                Direction::Bullish => low <= f.price_low,
                Direction::Bearish => high >= f.price_high,
            };
            if fully_filled {
                return Some(FvgState::Filled);
            }
            // Mitigated50 = price reaches the 50% midline.
            // For a bullish FVG that means low <= mid (price dipped into
            // the gap from above). For bearish, high >= mid (price popped
            // up into the gap from below). Symmetric check covers both.
            if low <= mid && high >= mid {
                return Some(FvgState::Mitigated50);
            }
        }
        FvgState::Mitigated50 => {
            // Filled = price punches through to the *opposite* side of the
            // gap (i.e. fully traversed it).
            match f.direction {
                Direction::Bullish => {
                    // bullish gap fills when price drops below price_low
                    if low <= f.price_low {
                        return Some(FvgState::Filled);
                    }
                }
                Direction::Bearish => {
                    if high >= f.price_high {
                        return Some(FvgState::Filled);
                    }
                }
            }
        }
        FvgState::Filled => {
            // After being filled, price has to come back from the OTHER
            // side. For a bullish FVG that means price now climbs back
            // above price_low (re-entering the gap from below). For
            // bearish: price drops back below price_high.
            match f.direction {
                Direction::Bullish => {
                    if high >= f.price_low {
                        return Some(FvgState::InvertedActive);
                    }
                }
                Direction::Bearish => {
                    if low <= f.price_high {
                        return Some(FvgState::InvertedActive);
                    }
                }
            }
        }
        FvgState::InvertedActive => {
            // The IFVG flips role. It's invalidated when price punches
            // through it again — for an inverted bullish FVG (now acting
            // as resistance) that means price closes above price_high; for
            // inverted bearish (now support), price drops below price_low.
            match f.direction {
                Direction::Bullish => {
                    if high >= f.price_high {
                        return Some(FvgState::InvertedMitigated);
                    }
                }
                Direction::Bearish => {
                    if low <= f.price_low {
                        return Some(FvgState::InvertedMitigated);
                    }
                }
            }
        }
        FvgState::InvertedMitigated => {
            // Terminal.
        }
    }
    None
}

impl Detector for FvgDetector {
    fn name(&self) -> &'static str {
        "fvg"
    }

    fn apply_param(&mut self, key: &str, value: &serde_json::Value) -> bool {
        match key {
            "min_size_pips" => {
                if let Some(v) = value.as_f64() {
                    self.cfg.min_size_pips = v;
                    true
                } else {
                    false
                }
            }
            "pip_size" => {
                if let Some(v) = value.as_f64() {
                    self.cfg.pip_size = v;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.tracked.clear();
        self.last_seen_close_ts = i64::MIN;
        self.last_transition.clear();
    }

    fn on_closed(&mut self, bars: &[Bar], _ctx: &super::DetectorCtx<'_>) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        if bars.is_empty() {
            return out;
        }
        let latest_ts = bars.last().unwrap().ts;

        // 1) Step every tracked FVG's state machine using the latest bar.
        let last = bars.last().unwrap();
        for f in self.tracked.iter_mut() {
            if let Some(ns) = step_state(f, last.low, last.high) {
                let same_bar_transition = self.last_transition.get(&f.id).copied();
                let allowed = match same_bar_transition {
                    Some((ts, reached)) if ts == last.ts => {
                        reached == FvgState::Mitigated50 && ns == FvgState::Filled
                    }
                    _ => true,
                };
                if !allowed {
                    continue;
                }
                if ns == FvgState::Filled && f.ts_filled.is_none() {
                    f.ts_filled = Some(last.ts);
                }
                f.state = ns;
                self.last_transition.insert(f.id.clone(), (last.ts, ns));
                // InvertedMitigated is terminal for state-machine stepping,
                // but remains in the structures map as immutable audit and
                // optional chart evidence. The SMT PDA selector only accepts
                // Active/Mitigated50, so an IFVG cannot gate a new SMT. Remove
                // it from `tracked` below because no further state transition
                // is possible.
                out.push(StructureEvent::Update(IctStructure::Fvg(f.clone())));
            }
        }
        // Drop terminally-mitigated entries from the tracked list.
        self.tracked
            .retain(|f| f.state != FvgState::InvertedMitigated);

        // 2) Try to detect a fresh 3-bar FVG anchored at the new last bar.
        if latest_ts > self.last_seen_close_ts {
            if let Some(f) = detect_three_bar(&self.symbol, self.tf, &self.cfg, bars) {
                // Avoid duplicate (same id already tracked).
                if !self.tracked.iter().any(|t| t.id == f.id) {
                    self.tracked.push(f.clone());
                    out.push(StructureEvent::New(IctStructure::Fvg(f)));
                }
            }
            self.last_seen_close_ts = latest_ts;
        }

        out
    }

    fn on_open(
        &mut self,
        current: &Bar,
        _history: &[Bar],
        _ctx: &super::DetectorCtx<'_>,
    ) -> Vec<StructureEvent> {
        // Don't create new structures from rolling bars; only progress states.
        let mut out = Vec::new();
        for f in self.tracked.iter_mut() {
            if let Some(ns) = step_state(f, current.low, current.high) {
                let same_bar_transition = self.last_transition.get(&f.id).copied();
                let allowed = match same_bar_transition {
                    Some((ts, reached)) if ts == current.ts => {
                        reached == FvgState::Mitigated50 && ns == FvgState::Filled
                    }
                    _ => true,
                };
                if !allowed {
                    continue;
                }
                if ns == FvgState::Filled && f.ts_filled.is_none() {
                    f.ts_filled = Some(current.ts);
                }
                f.state = ns;
                self.last_transition.insert(f.id.clone(), (current.ts, ns));
                out.push(StructureEvent::Update(IctStructure::Fvg(f.clone())));
            }
        }
        self.tracked
            .retain(|f| f.state != FvgState::InvertedMitigated);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Bar, Timeframe};

    fn bar(ts: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 0.0,
        }
    }
    fn cfg() -> FvgConfig {
        FvgConfig::default()
    }
    fn det() -> FvgDetector {
        FvgDetector::new("EURUSD", Timeframe::M5, cfg())
    }
    fn ctx() -> crate::detector::swing::SwingSeries {
        crate::detector::swing::SwingSeries::default()
    }
    macro_rules! call_closed {
        ($d:expr, $bars:expr) => {{
            let s = ctx();
            let c = crate::detector::DetectorCtx::new(&s);
            $d.on_closed(&$bars, &c)
        }};
    }
    macro_rules! call_open {
        ($d:expr, $cur:expr, $hist:expr) => {{
            let s = ctx();
            let c = crate::detector::DetectorCtx::new(&s);
            $d.on_open(&$cur, &$hist, &c)
        }};
    }

    fn ev_state(e: &StructureEvent) -> Option<(String, FvgState)> {
        match e {
            StructureEvent::New(IctStructure::Fvg(f))
            | StructureEvent::Update(IctStructure::Fvg(f)) => Some((f.id.clone(), f.state)),
            _ => None,
        }
    }
    fn count_new(events: &[StructureEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, StructureEvent::New(_)))
            .count()
    }

    #[test]
    fn bullish_formation() {
        // i-2 high < i low → bullish FVG
        let bars = [
            bar(100, 1.0, 1.05, 0.99, 1.04),
            bar(200, 1.04, 1.10, 1.03, 1.09),
            bar(300, 1.09, 1.12, 1.06, 1.11), // i.low=1.06 > i-2.high=1.05 → gap [1.05,1.06]
        ];
        let mut d = det();
        let events = call_closed!(d, bars);
        assert_eq!(count_new(&events), 1, "expected one New FVG");
        let (_id, st) = ev_state(events.first().unwrap()).unwrap();
        assert_eq!(st, FvgState::Active);
        let f = &d.tracked()[0];
        assert_eq!(f.direction, Direction::Bullish);
        assert!((f.price_low - 1.05).abs() < 1e-9);
        assert!((f.price_high - 1.06).abs() < 1e-9);
    }

    #[test]
    fn bearish_formation_then_mitigated_50() {
        // i-2.low > i.high → bearish gap [i.high, i-2.low]
        let bars = vec![
            bar(100, 1.20, 1.21, 1.15, 1.16),
            bar(200, 1.16, 1.17, 1.10, 1.11),
            bar(300, 1.11, 1.12, 1.06, 1.07), // gap [1.12, 1.15]
        ];
        let mut d = det();
        let mut all = call_closed!(d, bars);
        assert_eq!(count_new(&all), 1);
        // Push a 4th bar that touches mid (1.135) but doesn't fully fill.
        let mut bars2 = bars.clone();
        bars2.push(bar(400, 1.07, 1.14, 1.05, 1.13));
        all.extend(call_closed!(d, bars2));
        let (_, st) = ev_state(all.last().unwrap()).unwrap();
        assert_eq!(st, FvgState::Mitigated50);
    }

    #[test]
    fn full_lifecycle_bullish_to_inverted_mitigated() {
        // Bullish FVG: gap [1.05, 1.06].
        let mut bars = vec![
            bar(100, 1.00, 1.05, 0.99, 1.04),
            bar(200, 1.04, 1.10, 1.03, 1.09),
            bar(300, 1.09, 1.12, 1.06, 1.11),
        ];
        let mut d = det();
        let _ = call_closed!(d, bars);
        // bar 4: dip to mid (1.055) → Mitigated50
        bars.push(bar(400, 1.11, 1.13, 1.055, 1.06));
        let _ = call_closed!(d, bars);
        assert_eq!(d.tracked()[0].state, FvgState::Mitigated50);
        // bar 5: drop to 1.04 → Filled (low<=price_low)
        bars.push(bar(500, 1.06, 1.07, 1.04, 1.045));
        let _ = call_closed!(d, bars);
        assert_eq!(d.tracked()[0].state, FvgState::Filled);
        // bar 6: rally back into [1.05, 1.06] from below → InvertedActive
        bars.push(bar(600, 1.045, 1.055, 1.04, 1.05));
        let _ = call_closed!(d, bars);
        assert_eq!(d.tracked()[0].state, FvgState::InvertedActive);
        // bar 7: punch above price_high -> InvertedMitigated.
        // Now emits Update (not Invalidated) so the FVG remains
        // in the engine and can serve as a PDA candidate (S5.4).
        bars.push(bar(700, 1.05, 1.08, 1.05, 1.07));
        let events = call_closed!(d, bars);
        assert!(events.iter().any(|e| matches!(
            e,
            StructureEvent::Update(IctStructure::Fvg(ref f))
                if f.state == FvgState::InvertedMitigated
        )));
        assert!(
            d.tracked().is_empty(),
            "terminally mitigated FVGs are dropped from tracked"
        );
    }

    #[test]
    fn no_gap_no_event() {
        // overlapping bars, no gap
        let bars = [
            bar(100, 1.00, 1.05, 0.99, 1.03),
            bar(200, 1.03, 1.06, 1.02, 1.05),
            bar(300, 1.05, 1.07, 1.04, 1.06),
        ];
        let mut d = det();
        let events = call_closed!(d, bars);
        assert!(events.is_empty());
    }

    #[test]
    fn min_size_pips_filters_small_gaps() {
        // gap of 1 pip — drop with min_size_pips=5
        let bars = [
            bar(100, 1.0, 1.0500, 0.99, 1.04),
            bar(200, 1.04, 1.10, 1.03, 1.09),
            bar(300, 1.09, 1.12, 1.0501, 1.11), // 1 pip gap
        ];
        let mut d = FvgDetector::new(
            "EURUSD",
            Timeframe::M5,
            FvgConfig {
                min_size_pips: 5.0,
                pip_size: 0.0001,
            },
        );
        let events = call_closed!(d, bars);
        assert!(events.is_empty(), "1-pip gap below 5-pip threshold");
    }

    #[test]
    fn h4_fvg_rejects_vector_adjacent_bars_across_missing_parent_windows() {
        let tf = Timeframe::H4;
        let t0 = tf.boundary_align(1_786_000_000_000);
        let t1 = next_window_boundary(tf, t0);
        let missing = next_window_boundary(tf, t1);
        let t3 = next_window_boundary(tf, missing);
        let bars = [
            Bar {
                symbol: "TVC:DXY".into(),
                tf,
                ts: t0,
                open: 101.1,
                high: 101.3,
                low: 101.077,
                close: 101.2,
                volume: 0.0,
            },
            Bar {
                symbol: "TVC:DXY".into(),
                tf,
                ts: t1,
                open: 101.2,
                high: 101.3,
                low: 100.9,
                close: 101.0,
                volume: 0.0,
            },
            Bar {
                symbol: "TVC:DXY".into(),
                tf,
                ts: t3,
                open: 101.0,
                high: 101.011,
                low: 100.7,
                close: 100.8,
                volume: 0.0,
            },
        ];

        assert!(
            detect_three_bar("TVC:DXY", tf, &cfg(), &bars).is_none(),
            "a missing canonical H4 window must not manufacture a bearish PDA"
        );
    }

    #[test]
    fn h4_fvg_accepts_three_consecutive_canonical_parent_windows() {
        let tf = Timeframe::H4;
        let t0 = tf.boundary_align(1_786_000_000_000);
        let t1 = next_window_boundary(tf, t0);
        let t2 = next_window_boundary(tf, t1);
        let bars = [
            Bar {
                symbol: "TVC:DXY".into(),
                tf,
                ts: t0,
                open: 101.1,
                high: 101.3,
                low: 101.077,
                close: 101.2,
                volume: 0.0,
            },
            Bar {
                symbol: "TVC:DXY".into(),
                tf,
                ts: t1,
                open: 101.2,
                high: 101.3,
                low: 100.9,
                close: 101.0,
                volume: 0.0,
            },
            Bar {
                symbol: "TVC:DXY".into(),
                tf,
                ts: t2,
                open: 101.0,
                high: 101.011,
                low: 100.7,
                close: 100.8,
                volume: 0.0,
            },
        ];

        assert!(
            detect_three_bar("TVC:DXY", tf, &cfg(), &bars).is_some(),
            "the continuity guard must preserve a genuine consecutive H4 FVG"
        );
    }

    #[test]
    fn inverted_active_via_open_bar_progression() {
        // Set up Filled state then drive on_open.
        let mut bars = vec![
            bar(100, 1.00, 1.05, 0.99, 1.04),
            bar(200, 1.04, 1.10, 1.03, 1.09),
            bar(300, 1.09, 1.12, 1.06, 1.11),
        ];
        let mut d = det();
        let _ = call_closed!(d, bars);
        bars.push(bar(400, 1.11, 1.13, 1.055, 1.06)); // Mitigated50
        let _ = call_closed!(d, bars);
        bars.push(bar(500, 1.06, 1.07, 1.04, 1.045)); // Filled
        let _ = call_closed!(d, bars);
        // Rolling bar pushes into [1.05,1.06] from below (InvertedActive).
        let rolling = bar(600, 1.045, 1.055, 1.04, 1.05);
        let events = call_open!(d, rolling, bars);
        let st = events.iter().find_map(ev_state).map(|(_, s)| s);
        assert_eq!(st, Some(FvgState::InvertedActive));
    }

    #[test]
    fn active_gap_can_fill_directly_and_same_candle_cannot_invert_it() {
        let bars = vec![
            bar(100, 1.00, 1.05, 0.99, 1.04),
            bar(200, 1.04, 1.10, 1.03, 1.09),
            bar(300, 1.09, 1.12, 1.06, 1.11),
        ];
        let mut d = det();
        let _ = call_closed!(d, bars.clone());
        let rolling = bar(400, 1.11, 1.12, 1.04, 1.045);
        let first = call_open!(d, rolling.clone(), bars.clone());
        assert!(first.iter().any(|event| matches!(
            event,
            StructureEvent::Update(IctStructure::Fvg(f)) if f.state == FvgState::Filled
        )));
        assert_eq!(d.tracked()[0].ts_filled, Some(400));

        let _ = call_open!(d, rolling, bars);
        assert_eq!(d.tracked()[0].state, FvgState::Filled);
    }
}
