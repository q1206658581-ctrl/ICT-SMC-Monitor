use super::types::{Direction, GapState};

pub fn step_gap_state(
    direction: Direction,
    low: f64,
    high: f64,
    price_low: f64,
    price_high: f64,
    state: GapState,
) -> GapState {
    if state == GapState::Filled {
        return state;
    }
    let mid = (price_low + price_high) / 2.0;
    let touched_mid = low <= mid && high >= mid;
    let filled = match direction {
        Direction::Bullish => low <= price_low,
        Direction::Bearish => high >= price_high,
    };
    if filled {
        GapState::Filled
    } else if touched_mid {
        GapState::Mitigated50
    } else {
        state
    }
}
