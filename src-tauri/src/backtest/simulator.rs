use super::model::{Outcome, Simulation, MINUTE};
use ict_monitor::{
    llm::{DeterministicDecisionGuardrails, LlmDecisionDirection},
    types::Bar,
};

/// Pure M1 simulator. Bars are opening timestamps; `as_of` is the alert's
/// close boundary. Only bars starting at/after that boundary may be consumed.
/// Touch times are bar-close observation times, not invented tick times.
pub fn simulate(
    g: &DeterministicDecisionGuardrails,
    as_of: i64,
    price: f64,
    bars: &[Bar],
    window: usize,
    holding: usize,
) -> Simulation {
    simulate_exit(g, as_of, price, bars, window, holding, None)
}

pub fn simulate_exit(
    g: &DeterministicDecisionGuardrails,
    as_of: i64,
    price: f64,
    bars: &[Bar],
    window: usize,
    holding: usize,
    fixed_r: Option<f64>,
) -> Simulation {
    let Some(zone) = g.entry_zone.as_ref() else {
        return Simulation::empty(Outcome::Excluded, "missing_entry_zone");
    };
    let Some(stop) = g.invalidation_price else {
        return Simulation::empty(Outcome::Excluded, "missing_invalidation");
    };
    let target = match (fixed_r, g.targets.first()) {
        (Some(_), _) => price,
        (None, Some(target)) => target.price,
        (None, None) => return Simulation::empty(Outcome::Excluded, "missing_target"),
    };
    let bull = g.direction == LlmDecisionDirection::Bullish;
    if ![zone.low, zone.high, stop, target, price]
        .iter()
        .all(|v| v.is_finite())
        || fixed_r.is_some_and(|r| !r.is_finite() || r <= 0.0)
        || zone.low > zone.high
        || window == 0
        || holding == 0
        || match g.direction {
            LlmDecisionDirection::Bullish => {
                stop >= zone.low || (fixed_r.is_none() && target <= zone.high)
            }
            LlmDecisionDirection::Bearish => {
                stop <= zone.high || (fixed_r.is_none() && target >= zone.low)
            }
            _ => true,
        }
    {
        return Simulation::empty(Outcome::Excluded, "invalid_geometry_or_parameters");
    }
    if if bull { price <= stop } else { price >= stop } {
        return Simulation::empty(Outcome::Skip, "invalidated_at_anchor");
    }
    let mut result = Simulation::empty(Outcome::InsufficientData, "source_tail_before_resolution");
    if price >= zone.low && price <= zone.high {
        result.entry = Some(price);
        result.entry_ts = Some(as_of);
        result.time_to_entry_ms = Some(0);
        result.mfe_rr = Some(0.0);
        result.mae_rr = Some(0.0);
    }
    let mut expected = as_of;
    let mut waiting = 0;
    let mut held = 0;
    for bar in bars.iter().filter(|b| b.ts >= as_of) {
        if bar.ts != expected {
            result.outcome = Outcome::DataGap;
            result.reason = format!("missing_m1_at_{expected}");
            return result;
        }
        expected += MINUTE;
        if ![bar.open, bar.high, bar.low, bar.close]
            .iter()
            .all(|v| v.is_finite())
            || bar.low > bar.open.min(bar.close)
            || bar.high < bar.open.max(bar.close)
        {
            result.outcome = Outcome::DataGap;
            result.reason = format!("invalid_ohlc_at_{}", bar.ts);
            return result;
        }
        let stopped = if bull {
            bar.low <= stop
        } else {
            bar.high >= stop
        };
        if result.entry.is_none() {
            waiting += 1;
            let touched = bar.high >= zone.low && bar.low <= zone.high;
            if stopped {
                result.outcome = if touched {
                    Outcome::Ambiguous
                } else {
                    Outcome::Skip
                };
                result.reason = if touched {
                    "entry_and_stop_same_m1"
                } else {
                    "stop_before_entry"
                }
                .into();
                result.exit_ts = Some(expected);
                return result;
            }
            if touched {
                // Conservative fill boundary. With OHLC alone we cannot know
                // whether this candle's target/high/low preceded entry. Do not
                // award target or excursions from the entry candle.
                let entry = if bull { zone.high } else { zone.low };
                result.entry = Some(entry);
                result.entry_ts = Some(expected);
                result.time_to_entry_ms = Some(expected - as_of);
                result.mfe_rr = Some(0.0);
                result.mae_rr = Some(0.0);
                continue;
            }
            if waiting == window {
                result.outcome = Outcome::NoFill;
                result.reason = "entry_window_exhausted".into();
                result.exit_ts = Some(expected);
                return result;
            }
            continue;
        }
        held += 1;
        let entry = result.entry.unwrap();
        let risk = (entry - stop).abs();
        let sign = if bull { 1.0 } else { -1.0 };
        // Derive the fixed-R target from the actual fill, never the zone midpoint.
        let target = fixed_r.map(|r| entry + sign * r * risk).unwrap_or(target);
        let target_hit = if bull {
            bar.high >= target
        } else {
            bar.low <= target
        };
        // Excursions use only fully observed non-terminal holding candles.
        // On a terminal candle include the known exit, never a post-exit
        // high/low or unknowable favorable move before a same-bar stop.
        if stopped || target_hit {
            let exit_rr = if stopped {
                -1.0
            } else {
                (target - entry).abs() / risk
            };
            result.mfe_rr = Some(result.mfe_rr.unwrap().max(exit_rr.max(0.0)));
            result.mae_rr = Some(result.mae_rr.unwrap().max((-exit_rr).max(0.0)));
            result.outcome = if stopped { Outcome::Loss } else { Outcome::Win };
            result.reason = if stopped && target_hit {
                "target_and_stop_same_m1_loss"
            } else if stopped {
                "stop_first"
            } else {
                "target_first"
            }
            .into();
            result.realized_rr = Some(exit_rr);
            result.exit_ts = Some(expected);
            let elapsed = expected - result.entry_ts.unwrap();
            if stopped {
                result.time_to_stop_ms = Some(elapsed)
            } else {
                result.time_to_target_ms = Some(elapsed)
            }
            return result;
        }
        let favorable = if bull {
            bar.high - entry
        } else {
            entry - bar.low
        };
        let adverse = if bull {
            entry - bar.low
        } else {
            bar.high - entry
        };
        result.mfe_rr = Some(result.mfe_rr.unwrap().max(favorable / risk).max(0.0));
        result.mae_rr = Some(result.mae_rr.unwrap().max(adverse / risk).max(0.0));
        if held == holding {
            result.outcome = Outcome::Expired;
            result.reason = "holding_window_exhausted".into();
            result.exit_ts = Some(expected);
            result.floating_rr = Some(sign * (bar.close - entry) / risk);
            return result;
        }
    }
    result
}

/// Shared anchor-close execution for the paired stop comparison. No zone test.
/// Production guardrails are cloned; this does not modify production rules.
pub fn simulate_anchor(
    g: &DeterministicDecisionGuardrails,
    as_of: i64,
    price: f64,
    bars: &[Bar],
    holding: usize,
    multiple: f64,
) -> Simulation {
    let mut immediate = g.clone();
    immediate.entry_zone = Some(ict_monitor::llm::LlmEntryZone {
        low: price,
        high: price,
        source: "evaluation:shared_anchor_close".into(),
    });
    simulate_exit(&immediate, as_of, price, bars, 1, holding, Some(multiple))
}

/// User baseline: market entry at the anchor close; only C2 wick sets risk.
/// The anchor M1 has already closed and is not a holding candle.
pub fn simulate_user(
    g: &DeterministicDecisionGuardrails,
    symbol: &str,
    c2: &ict_monitor::detector::types::CandleRef,
    as_of: i64,
    price: f64,
    bars: &[Bar],
    holding: usize,
    buffer_points: u8,
    multiple: f64,
) -> super::model::UserResult {
    use super::model::UserResult;
    let bull = g.direction == LlmDecisionDirection::Bullish;
    let sign = if bull { 1.0 } else { -1.0 };
    let edge = if bull { c2.low } else { c2.high };
    let stop = edge
        - sign
            * f64::from(buffer_points)
            * 10_f64.powi(-(ict_monitor::llm::price_decimals(symbol) as i32));
    let mut result = UserResult {
        buffer_points,
        multiple,
        stop: stop.is_finite().then_some(stop),
        target: None,
        target1_in_user_r: None,
        simulation: Simulation::empty(Outcome::Excluded, "degenerate_stop"),
    };
    if ![price, c2.low, c2.high, stop].iter().all(|v| v.is_finite())
        || c2.low > c2.high
        || !matches!(
            g.direction,
            LlmDecisionDirection::Bullish | LlmDecisionDirection::Bearish
        )
        || sign * (price - edge) <= 0.0
        || sign * (price - stop) <= 0.0
    {
        return result;
    }
    let risk = (price - stop).abs();
    result.target = Some(price + sign * multiple * risk);
    result.target1_in_user_r = g
        .targets
        .first()
        .map(|t| sign * (t.price - price) / risk)
        .filter(|v| v.is_finite());
    let mut user = g.clone();
    user.invalidation_price = Some(stop);
    result.simulation = simulate_anchor(&user, as_of, price, bars, holding, multiple);
    result
}
