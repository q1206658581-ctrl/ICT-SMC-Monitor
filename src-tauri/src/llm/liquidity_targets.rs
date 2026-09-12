//! As-of L1 target sources. Never infer historical availability from today's
//! persisted `active`/`swept` flag or from a pivot's opening timestamp.
use super::{FacetContext, LlmEntryZone, LlmTarget};
use crate::{candidate::PackedEvidence, detector::types::Direction};

pub(super) fn collect(
    evidence: &PackedEvidence,
    symbol: &str,
    direction: Direction,
    zone: &LlmEntryZone,
    facet: &FacetContext,
    as_of: i64,
) -> Vec<LlmTarget> {
    let buy = direction == Direction::Bullish;
    let mut out = Vec::new();
    for structure in &facet.structures {
        if structure.symbol != symbol || structure.available_at_ts > as_of {
            continue;
        }
        let f = &structure.facts;
        let side = match structure.kind.as_str() {
            "pdh" => "buy_side",
            "pdl" => "sell_side",
            "equal_highs_lows" => {
                // Old rows identify pivot time only. Do not guess fractal_n
                // (it is configurable), or reuse wall-clock insertion time.
                if f.get("confirmed_at_ts").and_then(|v| v.as_i64()).is_none() {
                    continue;
                }
                f.get("side").and_then(|v| v.as_str()).unwrap_or("")
            }
            _ => continue,
        };
        if side != if buy { "buy_side" } else { "sell_side" } {
            continue;
        }
        let Some(price) = f
            .get("price")
            .and_then(|v| v.as_f64())
            .filter(|p| p.is_finite())
        else {
            continue;
        };
        if if buy {
            price <= zone.high
        } else {
            price >= zone.low
        } {
            continue;
        }
        // Sweeps/reversals are evidence of removal, never fresh target pools.
        let swept = facet.structures.iter().any(|s| {
            s.symbol == symbol
                && s.kind == "liquidity_sweep"
                && s.available_at_ts >= structure.available_at_ts
                && s.available_at_ts <= as_of
                && s.facts.get("side").and_then(|v| v.as_str()) == Some(side)
                && s.facts.get("level_price").and_then(|v| v.as_f64()) == Some(price)
        });
        if swept
            || !known_untouched(
                evidence,
                symbol,
                structure.available_at_ts,
                as_of,
                price,
                buy,
            )
        {
            continue;
        }
        out.push(LlmTarget {
            price: super::context::truncate_price(symbol, price),
            reason: format!(
                "program:L1:{}:{}:as_of={as_of}:{}",
                structure.kind,
                structure.structure_id,
                if structure.kind == "equal_highs_lows" {
                    "equal_high_low"
                } else {
                    "not_swept"
                }
            ),
        });
    }
    out
}

/// Prove a pool was not taken using complete closed-price coverage. Overlapping
/// TF bars can bridge the interval; a gap means Unknown, never NotSwept.
/// A crossed level is conservatively unavailable even if a separate sweep
/// detector did not emit a rejection candle. Future bars cannot affect this.
fn known_untouched(
    e: &PackedEvidence,
    symbol: &str,
    from: i64,
    as_of: i64,
    price: f64,
    buy: bool,
) -> bool {
    if from == as_of {
        return true;
    }
    let mut intervals = Vec::new();
    for bar in &e.market_bars {
        let end = bar.ts.saturating_add(bar.tf.duration_ms());
        if bar.symbol != symbol || bar.ts < from || end > as_of {
            continue;
        }
        if ![bar.open, bar.high, bar.low, bar.close]
            .iter()
            .all(|v| v.is_finite())
            || bar.low > bar.open.min(bar.close)
            || bar.high < bar.open.max(bar.close)
        {
            return false;
        }
        if if buy {
            bar.high > price
        } else {
            bar.low < price
        } {
            return false;
        }
        intervals.push((bar.ts, end));
    }
    intervals.sort_unstable();
    let mut cursor = from;
    for (start, end) in intervals {
        if start > cursor {
            return false;
        }
        cursor = cursor.max(end);
    }
    cursor >= as_of
}
