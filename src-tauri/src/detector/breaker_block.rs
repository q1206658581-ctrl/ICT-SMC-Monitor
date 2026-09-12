use crate::types::Bar;

use super::types::{structure_id, BreakerBlock, Direction, OrderBlock, ZoneState};

pub fn from_order_block(ob: &OrderBlock, confirm: &Bar) -> Option<BreakerBlock> {
    let direction = match ob.direction {
        Direction::Bullish if confirm.close < ob.price_low || confirm.low < ob.price_low => {
            Direction::Bearish
        }
        Direction::Bearish if confirm.close > ob.price_high || confirm.high > ob.price_high => {
            Direction::Bullish
        }
        _ => return None,
    };
    let id = structure_id(&[
        &ob.symbol,
        ob.tf.tag(),
        "breaker_block",
        &ob.id,
        &confirm.ts.to_string(),
    ]);
    Some(BreakerBlock {
        id,
        symbol: ob.symbol.clone(),
        tf: ob.tf,
        direction,
        source_ob_id: ob.id.clone(),
        ts_open: ob.ts_open,
        ts_confirm: confirm.ts,
        price_low: ob.price_low,
        price_high: ob.price_high,
        state: ZoneState::Active,
    })
}

pub fn step_breaker_state(mut breaker: BreakerBlock, bar: &Bar) -> BreakerBlock {
    if breaker.state == ZoneState::Invalidated || breaker.state == ZoneState::Mitigated {
        return breaker;
    }
    let overlaps = bar.high >= breaker.price_low && bar.low <= breaker.price_high;
    if overlaps && breaker.state == ZoneState::Active {
        breaker.state = ZoneState::Tested;
    }
    let invalidated = match breaker.direction {
        Direction::Bullish => bar.close < breaker.price_low,
        Direction::Bearish => bar.close > breaker.price_high,
    };
    if invalidated {
        breaker.state = ZoneState::Mitigated;
    }
    breaker
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::types::ObState;
    use crate::types::Timeframe;

    fn bar(ts: i64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            ts,
            open: c,
            high: h,
            low: l,
            close: c,
            volume: 0.0,
        }
    }

    fn ob(direction: Direction) -> OrderBlock {
        OrderBlock {
            id: "ob1".into(),
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            direction,
            ts_open: 0,
            ts_confirm: 1,
            price_low: 1.0,
            price_high: 1.1,
            state: ObState::Active,
        }
    }

    #[test]
    fn bullish_ob_breaks_to_bearish_breaker() {
        let b = from_order_block(&ob(Direction::Bullish), &bar(2, 1.05, 0.99, 0.99)).unwrap();
        assert_eq!(b.direction, Direction::Bearish);
    }

    #[test]
    fn bearish_ob_breaks_to_bullish_breaker() {
        let b = from_order_block(&ob(Direction::Bearish), &bar(2, 1.11, 1.05, 1.11)).unwrap();
        assert_eq!(b.direction, Direction::Bullish);
        assert_eq!(b.price_low, 1.0);
        assert_eq!(b.price_high, 1.1);
    }

    #[test]
    fn breaker_tests_and_mitigates() {
        let b = from_order_block(&ob(Direction::Bearish), &bar(2, 1.11, 1.05, 1.11)).unwrap();
        let b = step_breaker_state(b, &bar(3, 1.09, 1.05, 1.08));
        assert_eq!(b.state, ZoneState::Tested);
        let b = step_breaker_state(b, &bar(4, 1.05, 0.99, 0.99));
        assert_eq!(b.state, ZoneState::Mitigated);
    }

    #[test]
    fn bearish_breaker_tests_then_mitigates_on_reverse_close() {
        let b = from_order_block(&ob(Direction::Bullish), &bar(2, 1.05, 0.99, 0.99)).unwrap();
        let b = step_breaker_state(b, &bar(3, 1.08, 1.02, 1.04));
        assert_eq!(b.state, ZoneState::Tested);
        let b = step_breaker_state(b, &bar(4, 1.12, 1.07, 1.11));
        assert_eq!(b.state, ZoneState::Mitigated);
    }
}
