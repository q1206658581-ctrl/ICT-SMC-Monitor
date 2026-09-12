//! Order Block detector — §5.2.2.
//!
//! Bullish OB: the most recent **down-close** candle whose extreme low gets
//! left behind by displacement. We require: within `lookback` bars *after*
//! the OB candle, `max(high) - OB.high > displacement_atr_mult * ATR(14)`.
//! Bearish OB: mirror image.
//!
//! Range defaults to the full candle [low, high]; toggleable to body-only.
//! State machine: Active → Tested (price enters range) → Mitigated
//! (price closes through far side). Mitigated == Invalidated for M3.

use std::collections::VecDeque;

use super::types::{structure_id, Direction, IctStructure, ObState, OrderBlock, StructureEvent};
use super::{Detector, DetectorCtx};
use crate::types::{Bar, Timeframe};

#[derive(Clone, Debug)]
pub struct OrderBlockConfig {
    pub displacement_atr_mult: f64,
    pub lookback: usize,
    pub use_body_only: bool,
    pub atr_period: usize,
}
impl Default for OrderBlockConfig {
    fn default() -> Self {
        Self {
            displacement_atr_mult: 1.5,
            lookback: 5,
            use_body_only: false,
            atr_period: 14,
        }
    }
}

pub struct OrderBlockDetector {
    pub symbol: String,
    pub tf: Timeframe,
    pub cfg: OrderBlockConfig,
    tracked: Vec<OrderBlock>,
    /// True ranges window for incremental ATR.
    tr_window: VecDeque<f64>,
    last_seen_ts: i64,
    /// Indexes already evaluated for OB candidacy (i.e. ts of the candle).
    last_evaluated_ts: i64,
}

impl OrderBlockDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: OrderBlockConfig) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            tracked: Vec::new(),
            tr_window: VecDeque::new(),
            last_seen_ts: i64::MIN,
            last_evaluated_ts: i64::MIN,
        }
    }
    pub fn tracked(&self) -> &[OrderBlock] {
        &self.tracked
    }
}

fn atr(window: &VecDeque<f64>, period: usize) -> Option<f64> {
    if window.len() < period {
        return None;
    }
    let sum: f64 = window.iter().rev().take(period).sum();
    Some(sum / period as f64)
}

fn ob_range(b: &Bar, body_only: bool) -> (f64, f64) {
    if body_only {
        let lo = b.open.min(b.close);
        let hi = b.open.max(b.close);
        (lo, hi)
    } else {
        (b.low, b.high)
    }
}

fn ob_id(symbol: &str, tf: Timeframe, ts: i64, dir: Direction) -> String {
    let dir_tag = match dir {
        Direction::Bullish => "bull",
        Direction::Bearish => "bear",
    };
    structure_id(&[symbol, tf.tag(), "order_block", &ts.to_string(), dir_tag])
}

impl Detector for OrderBlockDetector {
    fn name(&self) -> &'static str {
        "order_block"
    }

    fn apply_param(&mut self, key: &str, value: &serde_json::Value) -> bool {
        match key {
            "displacement_atr_mult" => {
                if let Some(v) = value.as_f64() {
                    self.cfg.displacement_atr_mult = v;
                    true
                } else {
                    false
                }
            }
            "lookback" => {
                if let Some(v) = value.as_u64() {
                    self.cfg.lookback = v as usize;
                    true
                } else {
                    false
                }
            }
            "use_body_only" => {
                if let Some(v) = value.as_bool() {
                    self.cfg.use_body_only = v;
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
        self.tr_window.clear();
        self.last_seen_ts = i64::MIN;
        self.last_evaluated_ts = i64::MIN;
    }

    fn on_closed(&mut self, bars: &[Bar], _ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        let last = match bars.last() {
            Some(b) => b,
            None => return out,
        };
        if last.ts <= self.last_seen_ts {
            return out;
        }
        self.last_seen_ts = last.ts;

        // 1) Update TR window (needs prev bar).
        if bars.len() >= 2 {
            let prev = &bars[bars.len() - 2];
            let tr = (last.high - last.low)
                .max((last.high - prev.close).abs())
                .max((last.low - prev.close).abs());
            self.tr_window.push_back(tr);
            if self.tr_window.len() > 64 {
                self.tr_window.pop_front();
            }
        }

        // 2) Step state machine on every tracked OB using `last`.
        for ob in self.tracked.iter_mut() {
            match ob.state {
                ObState::Active => {
                    if last.low <= ob.price_high && last.high >= ob.price_low {
                        ob.state = ObState::Tested;
                        out.push(StructureEvent::Update(IctStructure::OrderBlock(ob.clone())));
                    }
                }
                ObState::Tested => {
                    let breached = match ob.direction {
                        Direction::Bullish => last.close < ob.price_low,
                        Direction::Bearish => last.close > ob.price_high,
                    };
                    if breached {
                        ob.state = ObState::Mitigated;
                        out.push(StructureEvent::Update(IctStructure::OrderBlock(ob.clone())));
                        out.push(StructureEvent::Invalidated {
                            id: ob.id.clone(),
                            kind: "order_block".into(),
                        });
                    }
                }
                ObState::Mitigated => {}
            }
        }
        self.tracked.retain(|o| o.state != ObState::Mitigated);

        // 3) New OB candidacy: scan the bar that is `lookback+1` bars back
        //    from the latest. Once we have `lookback` follow-through bars
        //    after a candidate, we can confirm displacement.
        let lookback = self.cfg.lookback.max(1);
        if bars.len() >= lookback + 2 {
            let candidate_idx = bars.len() - 1 - lookback;
            let candidate = &bars[candidate_idx];
            if candidate.ts > self.last_evaluated_ts {
                self.last_evaluated_ts = candidate.ts;
                let atr_v = atr(&self.tr_window, self.cfg.atr_period).unwrap_or(0.0);
                let thresh = atr_v * self.cfg.displacement_atr_mult;

                let (cand_lo, cand_hi) = ob_range(candidate, self.cfg.use_body_only);
                let after = &bars[candidate_idx + 1..];
                let max_high = after
                    .iter()
                    .map(|b| b.high)
                    .fold(f64::NEG_INFINITY, f64::max);
                let min_low = after.iter().map(|b| b.low).fold(f64::INFINITY, f64::min);

                // Bullish OB: down-close candidate, displacement to upside.
                if candidate.close < candidate.open && (max_high - cand_hi) > thresh && atr_v > 0.0
                {
                    let id = ob_id(&self.symbol, self.tf, candidate.ts, Direction::Bullish);
                    if !self.tracked.iter().any(|o| o.id == id) {
                        let ob = OrderBlock {
                            id: id.clone(),
                            symbol: self.symbol.clone(),
                            tf: self.tf,
                            direction: Direction::Bullish,
                            ts_open: candidate.ts,
                            ts_confirm: bars.last().unwrap().ts,
                            price_low: cand_lo,
                            price_high: cand_hi,
                            state: ObState::Active,
                        };
                        self.tracked.push(ob.clone());
                        out.push(StructureEvent::New(IctStructure::OrderBlock(ob)));
                    }
                }
                // Bearish OB: up-close candidate, displacement to downside.
                if candidate.close > candidate.open && (cand_lo - min_low) > thresh && atr_v > 0.0 {
                    let id = ob_id(&self.symbol, self.tf, candidate.ts, Direction::Bearish);
                    if !self.tracked.iter().any(|o| o.id == id) {
                        let ob = OrderBlock {
                            id: id.clone(),
                            symbol: self.symbol.clone(),
                            tf: self.tf,
                            direction: Direction::Bearish,
                            ts_open: candidate.ts,
                            ts_confirm: bars.last().unwrap().ts,
                            price_low: cand_lo,
                            price_high: cand_hi,
                            state: ObState::Active,
                        };
                        self.tracked.push(ob.clone());
                        out.push(StructureEvent::New(IctStructure::OrderBlock(ob)));
                    }
                }
            }
        }

        out
    }

    fn on_open(
        &mut self,
        current: &Bar,
        _history: &[Bar],
        _ctx: &super::DetectorCtx<'_>,
    ) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        for ob in self.tracked.iter_mut() {
            match ob.state {
                ObState::Active => {
                    if current.low <= ob.price_high && current.high >= ob.price_low {
                        ob.state = ObState::Tested;
                        out.push(StructureEvent::Update(IctStructure::OrderBlock(ob.clone())));
                    }
                }
                ObState::Tested => {
                    let breached = match ob.direction {
                        Direction::Bullish => {
                            current.close < ob.price_low || current.low < ob.price_low
                        }
                        Direction::Bearish => {
                            current.close > ob.price_high || current.high > ob.price_high
                        }
                    };
                    if breached {
                        ob.state = ObState::Mitigated;
                        out.push(StructureEvent::Update(IctStructure::OrderBlock(ob.clone())));
                        out.push(StructureEvent::Invalidated {
                            id: ob.id.clone(),
                            kind: "order_block".into(),
                        });
                    }
                }
                ObState::Mitigated => {}
            }
        }
        self.tracked.retain(|o| o.state != ObState::Mitigated);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::swing::SwingSeries;

    fn b(ts: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            symbol: "X".into(),
            tf: Timeframe::M5,
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 0.0,
        }
    }
    fn run(cfg: OrderBlockConfig, bars: &[Bar]) -> Vec<StructureEvent> {
        let mut s = SwingSeries::new(2);
        let mut d = OrderBlockDetector::new("X", Timeframe::M5, cfg);
        let mut all = Vec::new();
        for end in 1..=bars.len() {
            let slice = &bars[..end];
            s.on_closed_bar(slice);
            let ctx = DetectorCtx::new(&s);
            all.extend(d.on_closed(slice, &ctx));
        }
        all
    }

    fn sample_ob(direction: Direction, state: ObState) -> OrderBlock {
        OrderBlock {
            id: "ob-open".into(),
            symbol: "X".into(),
            tf: Timeframe::M5,
            direction,
            ts_open: 0,
            ts_confirm: 1,
            price_low: 1.0,
            price_high: 1.1,
            state,
        }
    }

    #[test]
    fn bullish_ob_after_displacement_up() {
        // Lots of small ATR baseline, then a down-close candidate followed
        // by a strong up displacement.
        let mut bars: Vec<Bar> = (1..=20)
            .map(|i| {
                let p = 1.10 + (i as f64) * 0.0001;
                b(i, p, p + 0.0005, p - 0.0005, p + 0.0001)
            })
            .collect();
        // candidate at ts=21 (down-close), then 5 bullish bars producing
        // displacement >> ATR.
        bars.push(b(21, 1.1010, 1.1012, 1.1000, 1.1002)); // down-close
        bars.push(b(22, 1.1002, 1.1050, 1.1000, 1.1048));
        bars.push(b(23, 1.1048, 1.1100, 1.1040, 1.1095));
        bars.push(b(24, 1.1095, 1.1180, 1.1090, 1.1170));
        bars.push(b(25, 1.1170, 1.1250, 1.1160, 1.1240));
        bars.push(b(26, 1.1240, 1.1320, 1.1230, 1.1310)); // <-- when this closes,
                                                          //     candidate_idx = 21
        let evs = run(
            OrderBlockConfig {
                lookback: 5,
                ..Default::default()
            },
            &bars,
        );
        let bull = evs.iter().find(|e| {
            matches!(e,
            StructureEvent::New(IctStructure::OrderBlock(o)) if o.direction == Direction::Bullish)
        });
        assert!(bull.is_some(), "expected bullish OB, got {:?}", evs);
    }

    #[test]
    fn bearish_ob_after_displacement_down() {
        let mut bars: Vec<Bar> = (1..=20)
            .map(|i| {
                let p = 1.20 - (i as f64) * 0.0001;
                b(i, p, p + 0.0005, p - 0.0005, p - 0.0001)
            })
            .collect();
        bars.push(b(21, 1.1980, 1.1990, 1.1975, 1.1988)); // up-close candidate
        bars.push(b(22, 1.1988, 1.1990, 1.1940, 1.1942));
        bars.push(b(23, 1.1942, 1.1945, 1.1880, 1.1885));
        bars.push(b(24, 1.1885, 1.1888, 1.1820, 1.1822));
        bars.push(b(25, 1.1822, 1.1825, 1.1760, 1.1762));
        bars.push(b(26, 1.1762, 1.1765, 1.1700, 1.1702));
        let evs = run(
            OrderBlockConfig {
                lookback: 5,
                ..Default::default()
            },
            &bars,
        );
        let bear = evs.iter().find(|e| {
            matches!(e,
            StructureEvent::New(IctStructure::OrderBlock(o)) if o.direction == Direction::Bearish)
        });
        assert!(bear.is_some(), "expected bearish OB, got {:?}", evs);
    }

    #[test]
    fn no_displacement_no_ob() {
        let bars: Vec<Bar> = (1..=30)
            .map(|i| {
                let p = 1.10 + (i as f64) * 0.00005;
                b(i, p, p + 0.0001, p - 0.0001, p + 0.00005)
            })
            .collect();
        let evs = run(
            OrderBlockConfig {
                lookback: 5,
                ..Default::default()
            },
            &bars,
        );
        let any = evs
            .iter()
            .any(|e| matches!(e, StructureEvent::New(IctStructure::OrderBlock(_))));
        assert!(!any);
    }

    #[test]
    fn on_open_tests_active_ob_and_invalidates_breached_ob() {
        let swings = SwingSeries::default();
        let mut d = OrderBlockDetector::new("X", Timeframe::M5, OrderBlockConfig::default());
        d.tracked
            .push(sample_ob(Direction::Bullish, ObState::Active));

        let evs = d.on_open(
            &b(2, 1.15, 1.16, 1.05, 1.12),
            &[],
            &DetectorCtx::new(&swings),
        );
        assert!(matches!(
            evs.first(),
            Some(StructureEvent::Update(IctStructure::OrderBlock(ob))) if ob.state == ObState::Tested
        ));

        let evs = d.on_open(
            &b(3, 1.04, 1.05, 0.98, 0.99),
            &[],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(|ev| matches!(
            ev,
            StructureEvent::Update(IctStructure::OrderBlock(ob)) if ob.state == ObState::Mitigated
        )));
        assert!(evs.iter().any(|ev| matches!(
            ev,
            StructureEvent::Invalidated { id, kind } if id == "ob-open" && kind == "order_block"
        )));
        assert!(d.tracked().is_empty());
    }
}
