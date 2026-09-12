//! Williams-fractal swing series + single-direction leg tracker.
//!
//! Shared by the MSS detector (needs swing highs/lows) and the CISD detector
//! (needs the *current leg origin's open* as the CISD line, §5.2.13). Keep
//! one instance per (symbol, tf) inside `IctEngine` so they stay in sync.
//!
//! Conventions:
//! - `fractal_n` defaults to 2 (a swing high needs the prior 2 highs lower
//!   AND the next 2 highs lower). This means the most recent swing is
//!   always lagging by `fractal_n` bars. **Don't cheat with future data.**
//! - A leg is the run of consecutive close-to-close delivery in one
//!   direction. Candle colour is deliberately ignored: a red candle may
//!   still deliver higher than the previous close, and vice versa. The leg's
//!   `origin_open` is the **open of the first bar in the leg** (NOT close —
//!   ICT's CISD line, §5.2.13).

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::types::Direction;
use crate::types::Bar;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SwingKind {
    High,
    Low,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Swing {
    pub ts: i64,
    pub price: f64,
    pub kind: SwingKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Leg {
    pub origin_ts: i64,
    pub origin_open: f64,
    pub direction: Direction,
    pub bars_in_leg: usize,
    pub current_extreme: f64,
}

#[derive(Clone, Debug, Default)]
pub struct SwingDelta {
    pub new_swing: Option<Swing>,
    pub leg_reversed: bool,
}

#[derive(Clone, Debug)]
pub struct SwingSeries {
    pub fractal_n: usize,
    pub swings: VecDeque<Swing>,
    pub current_leg: Option<Leg>,
    /// Most recently completed delivery leg. CISD must keep watching this
    /// leg's origin while the opposite leg develops; the actual close
    /// through the origin often happens several candles after the colour
    /// first changes.
    pub last_completed_leg: Option<Leg>,
    last_seen_ts: i64,
}

impl Default for SwingSeries {
    fn default() -> Self {
        Self::new(2)
    }
}

impl SwingSeries {
    pub fn new(fractal_n: usize) -> Self {
        Self {
            fractal_n: fractal_n.max(1),
            swings: VecDeque::new(),
            current_leg: None,
            last_completed_leg: None,
            last_seen_ts: i64::MIN,
        }
    }

    /// Last (most recently confirmed) swing of a given kind, if any.
    pub fn last_of(&self, kind: SwingKind) -> Option<&Swing> {
        self.swings.iter().rev().find(|s| s.kind == kind)
    }

    /// Two most recent swings (oldest, newer) of the given kind, if both exist.
    pub fn last_two(&self, kind: SwingKind) -> Option<(&Swing, &Swing)> {
        let mut iter = self.swings.iter().rev().filter(|s| s.kind == kind);
        let newer = iter.next()?;
        let older = iter.next()?;
        Some((older, newer))
    }

    /// Push a *closed* bar (caller is expected to feed the slice ending at
    /// the bar that just closed, oldest first). Returns SwingDelta describing
    /// what changed on this step.
    pub fn on_closed_bar(&mut self, bars: &[Bar]) -> SwingDelta {
        let mut delta = SwingDelta::default();
        if bars.is_empty() {
            return delta;
        }
        let last = bars.last().unwrap();
        if last.ts <= self.last_seen_ts {
            return delta;
        }
        self.last_seen_ts = last.ts;

        // 1) Look for newly-confirmed swing at index (len-1) - fractal_n.
        let n = self.fractal_n;
        if bars.len() >= 2 * n + 1 {
            let center = bars.len() - 1 - n;
            if let Some(sw) = check_fractal(bars, center, n) {
                // De-dup: don't re-emit the same swing
                if self.swings.back().map(|s| s.ts) != Some(sw.ts) {
                    self.swings.push_back(sw.clone());
                    if self.swings.len() > 256 {
                        self.swings.pop_front();
                    }
                    delta.new_swing = Some(sw);
                }
            }
        }

        // 2) Update / reverse current delivery leg. Delivery direction is a
        // close-to-close concept, not candle colour. Falling back to the
        // candle body is only necessary for the very first bar, where no
        // previous close exists yet.
        let leg_dir = bars
            .get(bars.len().saturating_sub(2))
            .filter(|_| bars.len() >= 2)
            .and_then(|previous| match (last.close, previous.close) {
                (c, p) if c > p => Some(Direction::Bullish),
                (c, p) if c < p => Some(Direction::Bearish),
                _ => None,
            })
            .or_else(|| match (last.close, last.open) {
                (c, o) if c > o => Some(Direction::Bullish),
                (c, o) if c < o => Some(Direction::Bearish),
                _ => None,
            });
        let leg_dir = leg_dir.unwrap_or_else(|| {
            // doji: keep prior leg direction if any, else default Bullish
            self.current_leg
                .as_ref()
                .map(|l| l.direction)
                .unwrap_or(Direction::Bullish)
        });

        match self.current_leg.as_mut() {
            Some(leg) if leg.direction == leg_dir => {
                leg.bars_in_leg += 1;
                leg.current_extreme = match leg.direction {
                    Direction::Bullish => leg.current_extreme.max(last.high),
                    Direction::Bearish => leg.current_extreme.min(last.low),
                };
            }
            _ => {
                self.last_completed_leg = self.current_leg.take();
                // Reversal (or first bar) — start a new leg whose origin is
                // *this* bar (the first bar of the new direction).
                self.current_leg = Some(Leg {
                    origin_ts: last.ts,
                    origin_open: last.open,
                    direction: leg_dir,
                    bars_in_leg: 1,
                    current_extreme: match leg_dir {
                        Direction::Bullish => last.high,
                        Direction::Bearish => last.low,
                    },
                });
                delta.leg_reversed = true;
            }
        }

        delta
    }
}

fn check_fractal(bars: &[Bar], center: usize, n: usize) -> Option<Swing> {
    let lo = center.checked_sub(n)?;
    let hi = center + n;
    if hi >= bars.len() {
        return None;
    }
    let pivot = &bars[center];
    let mut is_high = true;
    let mut is_low = true;
    for i in lo..=hi {
        if i == center {
            continue;
        }
        if bars[i].high >= pivot.high {
            is_high = false;
        }
        if bars[i].low <= pivot.low {
            is_low = false;
        }
    }
    if is_high {
        Some(Swing {
            ts: pivot.ts,
            price: pivot.high,
            kind: SwingKind::High,
        })
    } else if is_low {
        Some(Swing {
            ts: pivot.ts,
            price: pivot.low,
            kind: SwingKind::Low,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Timeframe;

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

    #[test]
    fn detect_swing_high_n2() {
        // 5 bars with center higher than both neighbors on each side.
        let bars = vec![
            b(1, 1.0, 1.05, 0.95, 1.04),
            b(2, 1.04, 1.10, 1.00, 1.08),
            b(3, 1.08, 1.20, 1.05, 1.18), // pivot high
            b(4, 1.18, 1.15, 1.10, 1.12),
            b(5, 1.12, 1.13, 1.08, 1.10),
        ];
        let mut s = SwingSeries::new(2);
        for end in 1..=bars.len() {
            s.on_closed_bar(&bars[..end]);
        }
        let last = s.last_of(SwingKind::High).expect("swing high");
        assert!((last.price - 1.20).abs() < 1e-9);
        assert_eq!(last.ts, 3);
    }

    #[test]
    fn leg_reversal_resets_origin() {
        // bullish closes, then bearish close → leg origin = first bearish bar.
        let bars = vec![
            b(1, 1.00, 1.05, 0.99, 1.04),
            b(2, 1.04, 1.10, 1.03, 1.09),
            b(3, 1.09, 1.12, 1.06, 1.07), // bearish close
        ];
        let mut s = SwingSeries::new(2);
        for end in 1..=bars.len() {
            s.on_closed_bar(&bars[..end]);
        }
        let leg = s.current_leg.as_ref().expect("leg");
        assert_eq!(leg.direction, Direction::Bearish);
        assert_eq!(leg.origin_ts, 3);
        assert!((leg.origin_open - 1.09).abs() < 1e-9);
        let completed = s
            .last_completed_leg
            .as_ref()
            .expect("completed bullish leg");
        assert_eq!(completed.direction, Direction::Bullish);
        assert_eq!(completed.origin_ts, 1);
        assert_eq!(completed.bars_in_leg, 2);
    }
}
