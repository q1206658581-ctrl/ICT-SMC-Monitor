//! Integration tests for the multi-timeframe aggregator.

use chrono::TimeZone;
use chrono_tz::America::New_York;
use ict_monitor::aggregator::SymbolAggregator;
use ict_monitor::types::{Bar, BarEvent, Timeframe};

fn m1(ts: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> Bar {
    Bar {
        symbol: "TEST:SYM".into(),
        tf: Timeframe::M1,
        ts,
        open: o,
        high: h,
        low: l,
        close: c,
        volume: v,
    }
}

fn ms_at_ny(year: i32, mo: u32, d: u32, h: u32, mn: u32) -> i64 {
    New_York
        .with_ymd_and_hms(year, mo, d, h, mn, 0)
        .single()
        .unwrap()
        .timestamp_millis()
}

fn closed_count(events: &[BarEvent], tf: Timeframe) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, BarEvent::BarClosed(b) if b.tf == tf))
        .count()
}

fn last_closed(events: &[BarEvent], tf: Timeframe) -> Option<&Bar> {
    events.iter().rev().find_map(|e| match e {
        BarEvent::BarClosed(b) if b.tf == tf => Some(b),
        _ => None,
    })
}

/// 60 1m bars at 00:00..01:00 UTC → 12×5m, 4×15m, 1×1h closed.
#[test]
fn sixty_1m_makes_twelve_fivemin_four_fifteenmin_one_hour() {
    let mut agg = SymbolAggregator::new("TEST:SYM");
    let mut all = Vec::new();
    let start = 1_700_000_000_000i64; // 2023-11-14 22:13:20 UTC arbitrary
                                      // align to a 1h boundary
    let aligned = Timeframe::H1.boundary_align(start);
    for i in 0..60 {
        let ts = aligned + i as i64 * 60_000;
        let bar = m1(
            ts,
            10.0 + i as f64 * 0.01,
            10.5 + i as f64 * 0.01,
            9.5 + i as f64 * 0.01,
            10.2 + i as f64 * 0.01,
            1.0,
        );
        let mut evs = agg.on_m1_event(&BarEvent::BarClosed(bar));
        all.append(&mut evs);
    }
    assert_eq!(closed_count(&all, Timeframe::M5), 12, "5m closes");
    assert_eq!(closed_count(&all, Timeframe::M15), 4, "15m closes");
    assert_eq!(closed_count(&all, Timeframe::H1), 1, "1h closes");

    // OHLC sanity on the 1h bar.
    let h1 = last_closed(&all, Timeframe::H1).expect("1h bar");
    assert_eq!(h1.ts, aligned);
    assert!((h1.open - 10.0).abs() < 1e-9);
    assert!((h1.close - (10.2 + 59.0 * 0.01)).abs() < 1e-9);
    assert!((h1.volume - 60.0).abs() < 1e-9);
}

/// Daily boundary lives at 17:00 NY-local. Feeding the 16:59 NY-local minute
/// then the 17:00 NY-local minute should close exactly one Daily.
#[test]
fn daily_closes_at_ny_17() {
    let mut agg = SymbolAggregator::new("TEST:SYM");
    // Use a January date — clearly EST (UTC-5).
    let t1659 = ms_at_ny(2026, 1, 15, 16, 59);
    let t1700 = ms_at_ny(2026, 1, 15, 17, 0);

    let mut events = Vec::new();
    events.append(&mut agg.on_m1_event(&BarEvent::BarClosed(m1(t1659, 1.0, 1.1, 0.9, 1.05, 1.0))));
    events.append(&mut agg.on_m1_event(&BarEvent::BarClosed(m1(t1700, 1.05, 1.2, 1.0, 1.15, 1.0))));

    // The bar covering 16:59 was the last minute of the IPDA day starting at
    // previous day's 17:00. So the Daily should close on its closure.
    assert_eq!(
        closed_count(&events, Timeframe::D1),
        1,
        "exactly one D1 closes"
    );
    let d1 = last_closed(&events, Timeframe::D1).unwrap();
    // d1 ts == 17:00 NY-local of the previous calendar day
    let expect = ms_at_ny(2026, 1, 14, 17, 0);
    assert_eq!(d1.ts, expect);
}

/// 4h windows stay on the TradingView chart grid 23/03/07/11/15/19 NY-local
/// across the 2026 spring DST switch (02:00 EST → 03:00 EDT).
#[test]
fn four_hour_alignment_spans_dst_spring_forward() {
    // Pick three 1m timestamps and check `boundary_align` returns the right
    // local-aligned UTC instant.
    // Saturday 21:30 EST belongs to the 19:00 chart window.
    let s1 = ms_at_ny(2026, 3, 7, 21, 30);
    let f1 = Timeframe::H4.boundary_align(s1);
    assert_eq!(f1, ms_at_ny(2026, 3, 7, 19, 0));

    // 04:30 EDT (right after the switch) belongs to the 03:00 window.
    let s2 = ms_at_ny(2026, 3, 8, 4, 30);
    let f2 = Timeframe::H4.boundary_align(s2);
    assert_eq!(f2, ms_at_ny(2026, 3, 8, 3, 0));

    // 09:00 EDT belongs to the 07:00 window.
    let s3 = ms_at_ny(2026, 3, 8, 9, 0);
    let f3 = Timeframe::H4.boundary_align(s3);
    assert_eq!(f3, ms_at_ny(2026, 3, 8, 7, 0));
}

#[test]
fn four_hour_alignment_spans_dst_fall_back() {
    // Before the repeated 01:00 hour, the prior 23:00 EDT anchor is used.
    let before = ms_at_ny(2026, 11, 1, 0, 30);
    assert_eq!(
        Timeframe::H4.boundary_align(before),
        ms_at_ny(2026, 10, 31, 23, 0)
    );

    // Once clocks have fallen back, 03:30 EST belongs to the 03:00 anchor.
    let after = ms_at_ny(2026, 11, 1, 3, 30);
    assert_eq!(
        Timeframe::H4.boundary_align(after),
        ms_at_ny(2026, 11, 1, 3, 0)
    );
}

/// First minute of an IPDA week (Sunday 17:00 NY) closes both the Daily,
/// Weekly and possibly Monthly that ended just before.
#[test]
fn week_open_at_sunday_17_ny_closes_prior_w_d_and_more() {
    let mut agg = SymbolAggregator::new("TEST:SYM");
    // Friday 2026-01-30 16:59 NY — last minute of week + daily + (maybe) month.
    // We only need the weekly + daily-closing demonstration here.
    let fri_1659 = ms_at_ny(2026, 1, 30, 16, 59);
    let sun_1700 = ms_at_ny(2026, 2, 1, 17, 0);

    let mut events = Vec::new();
    events
        .append(&mut agg.on_m1_event(&BarEvent::BarClosed(m1(fri_1659, 1.0, 1.1, 0.9, 1.0, 1.0))));
    events.append(&mut agg.on_m1_event(&BarEvent::BarClosed(m1(
        sun_1700, 1.0, 1.2, 0.95, 1.15, 1.0,
    ))));

    // Friday 16:59 close → Daily closes (ends Thu-17:00..Fri-17:00 day) and
    // Weekly closes (ends Sun-17:00 prev .. Fri-17:00).
    assert!(closed_count(&events, Timeframe::D1) >= 1, "D1 should close");
    assert_eq!(closed_count(&events, Timeframe::W1), 1, "exactly one W1");

    let w1 = last_closed(&events, Timeframe::W1).unwrap();
    // Week opened on Sunday 2026-01-25 17:00 NY.
    let expect_w_open = ms_at_ny(2026, 1, 25, 17, 0);
    assert_eq!(w1.ts, expect_w_open);
}
