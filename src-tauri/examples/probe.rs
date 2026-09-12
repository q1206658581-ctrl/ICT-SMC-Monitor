use ict_monitor::detector::types::*;
use ict_monitor::types::Timeframe;
fn main() {
    let s = IctStructure::KillZone(KillZoneSpan {
        id: "x".into(),
        symbol: "S".into(),
        tf: Timeframe::M1,
        kind: KillZoneKind::LondonOpen,
        label: "LOKZ".into(),
        ts_start: 1,
        ts_end: 2,
    });
    println!("{}", serde_json::to_string(&s).unwrap());
    let f = IctStructure::Fvg(Fvg {
        id: "y".into(),
        symbol: "S".into(),
        tf: Timeframe::M5,
        direction: Direction::Bullish,
        ts_open: 1,
        ts_confirm: 2,
        price_low: 1.0,
        price_high: 1.1,
        state: FvgState::Active,
        ts_filled: None,
        consumed_exit_ts: None,
    });
    println!("{}", serde_json::to_string(&f).unwrap());
}
