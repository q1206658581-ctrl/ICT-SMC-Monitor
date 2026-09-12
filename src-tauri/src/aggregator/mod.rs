//! Multi-timeframe bar aggregator.
//!
//! Consumes 1-minute `BarEvent`s from a `MarketDataProvider` and produces
//! synthetic higher-TF bars (5m / 15m / 1h / 4h / 1d / 1w / 1mo) following
//! the ICT/IPDA timing conventions documented in `docs/技术设计文档.md` §4.
//!
//! The aggregator itself is a pure state machine — no I/O, no async — so it
//! is trivially unit-testable. The driver task in `main.rs` plugs it into
//! `tokio::sync::broadcast` for downstream consumers (Tauri emit, future
//! detector engine, alert engine, ...).

use std::collections::{HashMap, VecDeque};

use crate::types::{Bar, BarEvent, Timeframe};

/// How many closed bars we keep in memory per (symbol, tf).
pub const HISTORY_RING_SIZE: usize = 1500;

/// One symbol's rolling state for every TF that 1m feeds into.
pub struct SymbolAggregator {
    symbol: String,
    /// Currently open (un-closed) bar per TF. Absent until first 1m feeds.
    rolling: HashMap<Timeframe, RollingBar>,
    /// Recent closed bars per TF.
    history: HashMap<Timeframe, VecDeque<Bar>>,
    /// Quote previews never alter the authoritative closed-minute accumulator.
    previews: HashMap<Timeframe, Bar>,
    latest_preview: Option<Bar>,
    last_closed_m1_ts: Option<i64>,
}

/// A higher-timeframe candle plus enough provenance to distinguish a real,
/// complete fixed-timeframe candle from an OHLC assembled across missing 1m
/// data. `last_source` also lets repeated updates for the same live minute
/// replace their volume contribution instead of adding it again on every tick.
#[derive(Clone)]
struct RollingBar {
    bar: Bar,
    first_m1_ts: i64,
    last_m1_ts: i64,
    minute_count: usize,
    contiguous: bool,
    last_source: Bar,
}

impl SymbolAggregator {
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            rolling: HashMap::new(),
            history: HashMap::new(),
            previews: HashMap::new(),
            latest_preview: None,
            last_closed_m1_ts: None,
        }
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// Snapshot of the most recent N closed bars on a given TF (oldest first).
    pub fn history(&self, tf: Timeframe) -> Vec<Bar> {
        self.history
            .get(&tf)
            .map(|d| d.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Current un-closed bar for a TF, if any.
    pub fn current(&self, tf: Timeframe) -> Option<&Bar> {
        self.previews
            .get(&tf)
            .or_else(|| self.rolling.get(&tf).map(|rolling| &rolling.bar))
    }

    /// Feed one 1-minute bar event. Returns the higher-TF events to emit
    /// (1m itself is **not** echoed back — caller already has it).
    pub fn on_m1_event(&mut self, evt: &BarEvent) -> Vec<BarEvent> {
        match evt {
            BarEvent::Historical(bars) => {
                // Replay each historical 1m as if it were a closed bar so the
                // higher TFs warm up. We don't emit historical events upward;
                // the caller handles that explicitly via warm-start API.
                for b in bars {
                    let _ = self.fold_closed_m1(b);
                }
                Vec::new()
            }
            BarEvent::BarUpdate(bar) => self.fold_update_m1(bar),
            BarEvent::BarClosed(bar) => self.fold_closed_m1(bar),
        }
    }

    /// Same as `on_m1_event` but feeds a single closed 1m bar — used by the
    /// SQLite warm-start path.
    pub fn warm_start_with_closed(&mut self, bar: &Bar) -> Vec<BarEvent> {
        self.fold_closed_m1(bar)
    }

    fn fold_update_m1(&mut self, m1: &Bar) -> Vec<BarEvent> {
        debug_assert_eq!(m1.tf, Timeframe::M1);
        if self.last_closed_m1_ts.is_some_and(|ts| m1.ts <= ts)
            || self
                .latest_preview
                .as_ref()
                .is_some_and(|bar| m1.ts < bar.ts)
        {
            return Vec::new();
        }
        self.latest_preview = Some(m1.clone());
        let mut out = Vec::new();
        for tf in Timeframe::higher_than_m1() {
            let aligned_ts = tf.boundary_align(m1.ts);
            let mut preview = self
                .rolling
                .get(tf)
                .filter(|rolling| rolling.bar.ts == aligned_ts)
                .cloned()
                .unwrap_or_else(|| new_rolling_at(&self.symbol, *tf, aligned_ts, m1));
            merge_source(&mut preview, m1);
            self.previews.insert(*tf, preview.bar.clone());
            out.push(BarEvent::BarUpdate(preview.bar));
        }
        // Even crossing a boundary is only a preview. A delayed authoritative
        // close can still finish the previous window without rollback.
        out
    }

    fn fold_closed_m1(&mut self, m1: &Bar) -> Vec<BarEvent> {
        debug_assert_eq!(m1.tf, Timeframe::M1);
        if self.last_closed_m1_ts.is_some_and(|ts| m1.ts <= ts) {
            return Vec::new();
        }
        self.last_closed_m1_ts = Some(m1.ts);
        self.previews.clear();
        if self
            .latest_preview
            .as_ref()
            .is_some_and(|bar| bar.ts <= m1.ts)
        {
            self.latest_preview = None;
        }
        let mut out = Vec::new();
        // The closed 1m timestamp is the OPEN of that minute. The minute's
        // CLOSE is m1.ts + 60_000ms - 1. Whether higher-TF bar closes after
        // *this* 1m is therefore: does (m1.ts + 60_000) start a new aligned
        // window on that TF?
        let m1_close_next = m1.ts + Timeframe::M1.duration_ms();
        for tf in Timeframe::higher_than_m1() {
            let cur_window = tf.boundary_align(m1.ts);
            let next_window = tf.boundary_align(m1_close_next);

            // Merge this 1m into the rolling bar of this TF.
            let merged = match self.rolling.remove(tf) {
                Some(mut prev) if prev.bar.ts == cur_window => {
                    merge_source(&mut prev, m1);
                    prev
                }
                Some(prev) => {
                    let previous_window_end = next_window_boundary(*tf, prev.bar.ts);
                    if window_is_publishable(&self.symbol, *tf, &prev, previous_window_end) {
                        self.push_history(*tf, prev.bar.clone());
                        out.push(BarEvent::BarClosed(prev.bar));
                    } else {
                        tracing::warn!(
                            symbol = %self.symbol, tf = %tf.tag(),
                            window_ts = prev.bar.ts,
                            minutes = prev.minute_count,
                            "dropping incomplete stale aggregated window"
                        );
                    }
                    new_rolling_at(&self.symbol, *tf, cur_window, m1)
                }
                None => new_rolling_at(&self.symbol, *tf, cur_window, m1),
            };

            if next_window != cur_window {
                // Window boundary crossed → this rolling bar is now closed.
                if window_is_publishable(&self.symbol, *tf, &merged, next_window) {
                    self.push_history(*tf, merged.bar.clone());
                    out.push(BarEvent::BarClosed(merged.bar));
                } else {
                    tracing::warn!(
                        symbol = %self.symbol, tf = %tf.tag(),
                        window_ts = merged.bar.ts,
                        minutes = merged.minute_count,
                        "dropping incomplete aggregated close"
                    );
                }
                // Don't pre-create the next rolling; it'll be born on the
                // first 1m of the next window.
            } else {
                out.push(BarEvent::BarUpdate(merged.bar.clone()));
                self.rolling.insert(*tf, merged);
            }
        }
        if let Some(preview) = self.latest_preview.clone() {
            out.extend(self.fold_update_m1(&preview));
        }
        out
    }

    fn push_history(&mut self, tf: Timeframe, bar: Bar) {
        let ring = self.history.entry(tf).or_insert_with(VecDeque::new);
        ring.push_back(bar);
        while ring.len() > HISTORY_RING_SIZE {
            ring.pop_front();
        }
    }
}

fn new_rolling_at(symbol: &str, tf: Timeframe, aligned_ts: i64, src: &Bar) -> RollingBar {
    RollingBar {
        bar: Bar {
            symbol: symbol.to_string(),
            tf,
            ts: aligned_ts,
            open: src.open,
            high: src.high,
            low: src.low,
            close: src.close,
            volume: src.volume,
        },
        first_m1_ts: src.ts,
        last_m1_ts: src.ts,
        minute_count: 1,
        contiguous: true,
        last_source: src.clone(),
    }
}

fn merge_source(acc: &mut RollingBar, m1: &Bar) {
    if m1.ts == acc.last_m1_ts {
        // Replace the contribution of the still-open minute. TradingView's
        // cumulative per-bar volume must not be added once per tick and then
        // added again when the minute closes.
        acc.bar.volume += m1.volume - acc.last_source.volume;
    } else if m1.ts > acc.last_m1_ts {
        if m1.ts != acc.last_m1_ts + Timeframe::M1.duration_ms() {
            acc.contiguous = false;
        }
        acc.minute_count += 1;
        acc.bar.volume += m1.volume;
    } else {
        acc.contiguous = false;
        return;
    }
    if m1.high > acc.bar.high {
        acc.bar.high = m1.high;
    }
    if m1.low < acc.bar.low {
        acc.bar.low = m1.low;
    }
    acc.bar.close = m1.close;
    acc.last_m1_ts = m1.ts;
    acc.last_source = m1.clone();
}

fn requires_complete_minutes(tf: Timeframe) -> bool {
    matches!(
        tf,
        Timeframe::M5 | Timeframe::M15 | Timeframe::M30 | Timeframe::H1 | Timeframe::H4
    )
}

fn window_is_publishable(
    symbol: &str,
    tf: Timeframe,
    rolling: &RollingBar,
    next_window: i64,
) -> bool {
    if !requires_complete_minutes(tf) {
        return true;
    }
    let expected_minutes =
        expected_market_bar_opens(symbol, Timeframe::M1, rolling.bar.ts, next_window);
    let Some(first_expected) = expected_minutes.first() else {
        return false;
    };
    let Some(last_expected) = expected_minutes.last() else {
        return false;
    };
    rolling.contiguous
        && rolling.first_m1_ts == *first_expected
        && rolling.last_m1_ts == *last_expected
        && rolling.minute_count == expected_minutes.len()
}

/// Return the next canonical candle boundary. Detector formation guards use
/// the same function as aggregation so H4 DST transitions cannot disagree
/// about whether two parent candles are adjacent.
pub fn next_window_boundary(tf: Timeframe, window: i64) -> i64 {
    if matches!(
        tf,
        Timeframe::M1 | Timeframe::M5 | Timeframe::M15 | Timeframe::M30 | Timeframe::H1
    ) {
        return window + tf.duration_ms();
    }

    // H4/D1/W1/MN1 are New-York civil-time candles. Their UTC duration can
    // be one hour shorter or longer around DST, and month length is not
    // fixed at all. Probe with an H1 cursor until the canonical bucket
    // changes instead of adding the nominal `duration_ms()` value.
    let max_probe_hours = match tf {
        Timeframe::H4 => 6,
        Timeframe::D1 => 26,
        Timeframe::W1 => 8 * 24,
        Timeframe::MN1 => 32 * 24,
        _ => unreachable!("fixed intraday timeframes returned above"),
    };
    let step = Timeframe::H1.duration_ms();
    let mut probe = window + step;
    for _ in 0..max_probe_hours {
        let aligned = tf.boundary_align(probe);
        if aligned != window {
            return aligned;
        }
        probe += step;
    }
    window + tf.duration_ms()
}

fn market_is_open(symbol: &str, ts_ms: i64) -> bool {
    if !is_forex_like(symbol) {
        return true;
    }
    use chrono::{Datelike, TimeZone, Timelike, Utc, Weekday};
    use chrono_tz::America::New_York;
    let Some(utc) = Utc.timestamp_millis_opt(ts_ms).single() else {
        return false;
    };
    let ny = utc.with_timezone(&New_York);
    // Christmas Day and New Year's Day are full FX market holidays.  Treating
    // their canonical buckets as expected data makes the recovery loop chase
    // bars that no provider can return after every cold start.
    if matches!((ny.month(), ny.day()), (12, 25) | (1, 1)) {
        return false;
    }
    match ny.weekday() {
        Weekday::Sat => false,
        Weekday::Sun => ny.hour() >= 17,
        Weekday::Fri => ny.hour() < 17,
        _ => true,
    }
}

/// Canonical source-bar opens that may exist inside `[start_ts, end_ts)`.
///
/// Aggregation and downstream evidence validation must use the same market
/// calendar. In particular, the final Friday H4 window can legitimately
/// contain fewer than four H1 children because FX closes at 17:00 New York.
pub fn expected_market_bar_opens(
    symbol: &str,
    tf: Timeframe,
    start_ts: i64,
    end_ts: i64,
) -> Vec<i64> {
    let duration = tf.duration_ms();
    if duration <= 0 || end_ts <= start_ts || (end_ts - start_ts) % duration != 0 {
        return Vec::new();
    }
    (start_ts..end_ts)
        .step_by(duration as usize)
        .filter(|ts| market_is_open(symbol, *ts))
        .collect()
}

fn is_forex_like(symbol: &str) -> bool {
    let (exchange, ticker) = symbol
        .split_once(':')
        .map(|(exchange, ticker)| (Some(exchange), ticker))
        .unwrap_or((None, symbol));
    if ticker == "DXY" {
        return true;
    }
    let currency_like = ticker.len() == 6 && ticker.chars().all(|c| c.is_ascii_uppercase());
    currency_like
        && match exchange {
            // Crypto tickers such as COINBASE:BTCUSD also have six uppercase
            // characters but trade continuously; never apply the FX weekend
            // calendar based on ticker length alone.
            Some("OANDA" | "FX" | "FX_IDC" | "FOREXCOM" | "SAXO") => true,
            Some(_) => false,
            None => true,
        }
}

/// Re-aggregate 4h bars from a slice of (clean) 1h bars.
///
/// TradingView emits TVC:DXY (and sometimes OANDA forex) 4h bars on an
/// inconsistent grid, which breaks cross-symbol SMT alignment. 1h bars are
/// always clean, and aggregating 4h from 1h yields OHLC identical to the
/// 1m->4h path (an hour's high/low already capture its minute extremes), so
/// this produces canonical-grid 4h bars on demand.
///
/// Input is re-sorted ascending by `ts` defensively. Only fully closed 4h
/// windows (`window_start + 4h <= now_ms`) are emitted; the still-open window
/// is left to the live aggregator's rolling bar.
pub fn aggregate_h4_from_h1(h1: &[Bar], symbol: &str, now_ms: i64) -> Vec<Bar> {
    use std::collections::BTreeMap;
    let h4 = Timeframe::H4;
    let mut sorted: Vec<&Bar> = h1.iter().collect();
    sorted.sort_by_key(|b| b.ts);
    let mut groups: BTreeMap<i64, Vec<&Bar>> = BTreeMap::new();
    for b in &sorted {
        let w = h4.boundary_align(b.ts);
        groups.entry(w).or_default().push(*b);
    }
    groups
        .into_iter()
        .filter_map(|(window, bars)| {
            let next = next_window_boundary(Timeframe::H4, window);
            if next > now_ms {
                return None;
            }
            let expected_hours = expected_market_bar_opens(symbol, Timeframe::H1, window, next);
            if expected_hours.is_empty()
                || bars.len() != expected_hours.len()
                || bars
                    .iter()
                    .zip(&expected_hours)
                    .any(|(bar, ts)| bar.ts != *ts)
            {
                return None;
            }
            let first = bars[0];
            let last = bars[bars.len() - 1];
            Some(Bar {
                symbol: symbol.to_string(),
                tf: h4,
                ts: window,
                open: first.open,
                high: bars
                    .iter()
                    .map(|bar| bar.high)
                    .fold(f64::NEG_INFINITY, f64::max),
                low: bars.iter().map(|bar| bar.low).fold(f64::INFINITY, f64::min),
                close: last.close,
                volume: bars.iter().map(|bar| bar.volume).sum(),
            })
        })
        .collect()
}

/// Return true when the retained H1 slice contains an internal, fully-closed
/// canonical H4 window that cannot be rebuilt strictly. The partial left edge
/// of a bounded history query is ignored; it is not evidence of a provider
/// hole.
pub fn has_incomplete_h4_window(h1: &[Bar], symbol: &str, now_ms: i64) -> bool {
    use std::collections::BTreeMap;

    if h1.len() < 2 {
        return false;
    }
    let mut groups: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for bar in h1 {
        groups
            .entry(Timeframe::H4.boundary_align(bar.ts))
            .or_default()
            .push(bar.ts);
    }
    for timestamps in groups.values_mut() {
        timestamps.sort_unstable();
        timestamps.dedup();
    }

    let first_h1_ts = h1.iter().map(|bar| bar.ts).min().unwrap_or_default();
    let last_h1_ts = h1.iter().map(|bar| bar.ts).max().unwrap_or_default();
    let mut window = Timeframe::H4.boundary_align(first_h1_ts);
    let last_window = Timeframe::H4.boundary_align(last_h1_ts);
    while window <= last_window {
        let next = next_window_boundary(Timeframe::H4, window);
        if next > now_ms {
            break;
        }
        let expected = expected_market_bar_opens(symbol, Timeframe::H1, window, next);
        if !expected.is_empty() {
            // A LIMIT query commonly starts inside its first H4 window. Only
            // diagnose that edge when its first expected H1 candle is present.
            let left_edge_is_auditable = window != Timeframe::H4.boundary_align(first_h1_ts)
                || expected.first() == Some(&first_h1_ts);
            if left_edge_is_auditable {
                let actual = groups.get(&window).map(Vec::as_slice).unwrap_or(&[]);
                if actual != expected.as_slice() {
                    return true;
                }
            }
        }
        window = next;
    }
    false
}

fn prices_match(a: f64, b: f64) -> bool {
    let scale = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() <= scale * 1e-9
}

fn native_h4_matches_available_h1(native: &Bar, h1: &[&Bar], expected_hours: &[i64]) -> bool {
    if h1.is_empty() || expected_hours.is_empty() {
        return false;
    }
    if h1.iter().any(|bar| !expected_hours.contains(&bar.ts)) {
        return false;
    }
    let available_high = h1
        .iter()
        .map(|bar| bar.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let available_low = h1.iter().map(|bar| bar.low).fold(f64::INFINITY, f64::min);
    if native.high + 1e-9 < available_high || native.low - 1e-9 > available_low {
        return false;
    }
    if let Some(first) = h1
        .iter()
        .find(|bar| Some(&bar.ts) == expected_hours.first())
    {
        if !prices_match(native.open, first.open) {
            return false;
        }
    }
    if let Some(last) = h1.iter().find(|bar| Some(&bar.ts) == expected_hours.last()) {
        if !prices_match(native.close, last.close) {
            return false;
        }
    }
    true
}

/// Rebuild canonical H4 bars from H1, using authoritative TV-native H4 rows
/// only for strict H1 windows that are incomplete. Native rows must already
/// sit on the canonical chart grid and must agree with every available H1
/// child, so an off-grid provider candle can never leak into SMT/FVG replay.
pub fn aggregate_h4_with_native_fallback(
    h1: &[Bar],
    native_h4: &[Bar],
    symbol: &str,
    now_ms: i64,
) -> Vec<Bar> {
    use std::collections::BTreeMap;

    let mut by_window: BTreeMap<i64, Bar> = aggregate_h4_from_h1(h1, symbol, now_ms)
        .into_iter()
        .map(|bar| (bar.ts, bar))
        .collect();
    let mut h1_groups: BTreeMap<i64, Vec<&Bar>> = BTreeMap::new();
    for bar in h1 {
        h1_groups
            .entry(Timeframe::H4.boundary_align(bar.ts))
            .or_default()
            .push(bar);
    }
    for bars in h1_groups.values_mut() {
        bars.sort_by_key(|bar| bar.ts);
    }

    for native in native_h4 {
        let window = native.ts;
        if native.tf != Timeframe::H4
            || Timeframe::H4.boundary_align(window) != window
            || by_window.contains_key(&window)
        {
            continue;
        }
        let next = next_window_boundary(Timeframe::H4, window);
        if next > now_ms {
            continue;
        }
        let expected = expected_market_bar_opens(symbol, Timeframe::H1, window, next);
        let available = h1_groups.get(&window).map(Vec::as_slice).unwrap_or(&[]);
        if available.len() >= expected.len()
            || !native_h4_matches_available_h1(native, available, &expected)
        {
            continue;
        }
        by_window.insert(window, native.clone());
    }

    by_window.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h1(symbol: &str, ts: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> Bar {
        Bar {
            symbol: symbol.to_string(),
            tf: Timeframe::H1,
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    #[test]
    fn aggregates_four_hours_into_one_h4_on_grid() {
        let w = Timeframe::H4.boundary_align(1_700_000_000_000);
        let bars = vec![
            h1("DXY", w, 100.0, 100.5, 99.8, 100.2, 10.0),
            h1("DXY", w + 3_600_000, 100.2, 101.0, 100.1, 100.8, 12.0),
            h1("DXY", w + 7_200_000, 100.8, 101.2, 100.7, 101.0, 8.0),
            h1("DXY", w + 10_800_000, 101.0, 101.1, 100.9, 100.95, 9.0),
        ];
        let now = w + 4 * 3_600_000;
        let out = aggregate_h4_from_h1(&bars, "DXY", now);
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(b.ts, w);
        assert_eq!(b.tf, Timeframe::H4);
        assert_eq!(b.open, 100.0);
        assert_eq!(b.high, 101.2);
        assert_eq!(b.low, 99.8);
        assert_eq!(b.close, 100.95);
        assert_eq!(b.volume, 39.0);
    }

    #[test]
    fn open_window_is_dropped() {
        let w = Timeframe::H4.boundary_align(2_000_000_000_000);
        let bars = vec![h1("DXY", w, 100.0, 100.5, 99.8, 100.2, 10.0)];
        // now sits inside the same 4h window -> not closed yet
        let out = aggregate_h4_from_h1(&bars, "DXY", w + 1_800_000);
        assert!(out.is_empty());
    }

    #[test]
    fn sparse_window_is_dropped() {
        let w0 = Timeframe::H4.boundary_align(1_700_000_000_000);
        let w1 = w0 + 4 * 3_600_000;
        let now = w1 + 4 * 3_600_000;
        // w1 has only one 1h bar. Publishing it as a full 4h candle would
        // corrupt SMT/PDA extrema, so only the complete w0 window survives.
        let bars = vec![
            h1("DXY", w0, 1.0, 2.0, 0.5, 1.5, 1.0),
            h1("DXY", w0 + 3_600_000, 1.5, 2.5, 1.4, 2.0, 1.0),
            h1("DXY", w0 + 7_200_000, 2.0, 2.2, 1.9, 2.1, 1.0),
            h1("DXY", w0 + 10_800_000, 2.1, 2.3, 2.0, 2.2, 1.0),
            h1("DXY", w1, 5.0, 5.5, 4.9, 5.2, 3.0),
        ];
        let out = aggregate_h4_from_h1(&bars, "DXY", now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ts, w0);
    }

    #[test]
    fn native_h4_repairs_an_internal_h1_hole_on_the_canonical_grid() {
        let previous = Timeframe::H4.boundary_align(1_700_000_000_000);
        let w = next_window_boundary(Timeframe::H4, previous);
        let h1 = vec![
            h1("TVC:DXY", previous, 99.60, 99.65, 99.55, 99.61, 1.0),
            h1(
                "TVC:DXY",
                previous + 3_600_000,
                99.61,
                99.66,
                99.56,
                99.62,
                1.0,
            ),
            h1(
                "TVC:DXY",
                previous + 2 * 3_600_000,
                99.62,
                99.67,
                99.57,
                99.63,
                1.0,
            ),
            h1(
                "TVC:DXY",
                previous + 3 * 3_600_000,
                99.63,
                99.68,
                99.58,
                99.64,
                1.0,
            ),
            h1(
                "TVC:DXY",
                w + 2 * 3_600_000,
                99.45,
                99.50,
                99.40,
                99.43,
                1.0,
            ),
            h1(
                "TVC:DXY",
                w + 3 * 3_600_000,
                99.43,
                99.46,
                99.35,
                99.39,
                1.0,
            ),
        ];
        let native = Bar {
            symbol: "TVC:DXY".into(),
            tf: Timeframe::H4,
            ts: w,
            open: 99.49,
            high: 99.51,
            low: 99.35,
            close: 99.39,
            volume: 4.0,
        };
        let now = next_window_boundary(Timeframe::H4, w);

        assert!(has_incomplete_h4_window(&h1, "TVC:DXY", now));
        assert!(!aggregate_h4_from_h1(&h1, "TVC:DXY", now)
            .iter()
            .any(|bar| bar.ts == w));
        let repaired = aggregate_h4_with_native_fallback(&h1, &[native.clone()], "TVC:DXY", now);
        let recovered = repaired.last().expect("native H4 fallback");
        assert_eq!(recovered.ts, native.ts);
        assert_eq!(recovered.open, native.open);
        assert_eq!(recovered.high, native.high);
        assert_eq!(recovered.low, native.low);
        assert_eq!(recovered.close, native.close);
    }

    #[test]
    fn native_h4_fallback_rejects_a_row_that_disagrees_with_available_h1() {
        let w = Timeframe::H4.boundary_align(1_700_000_000_000);
        let h1 = vec![
            h1(
                "TVC:DXY",
                w + 2 * 3_600_000,
                99.45,
                99.50,
                99.40,
                99.43,
                1.0,
            ),
            h1(
                "TVC:DXY",
                w + 3 * 3_600_000,
                99.43,
                99.46,
                99.35,
                99.39,
                1.0,
            ),
        ];
        let bad_native = Bar {
            symbol: "TVC:DXY".into(),
            tf: Timeframe::H4,
            ts: w,
            open: 99.49,
            high: 99.48,
            low: 99.35,
            close: 99.38,
            volume: 4.0,
        };
        let now = next_window_boundary(Timeframe::H4, w);
        assert!(aggregate_h4_with_native_fallback(&h1, &[bad_native], "TVC:DXY", now).is_empty());
    }

    #[test]
    fn repeated_live_minute_replaces_volume_instead_of_accumulating_ticks() {
        let mut agg = SymbolAggregator::new("OANDA:EURUSD");
        let start = Timeframe::M5.boundary_align(1_700_000_000_000);
        let mut tick = Bar {
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M1,
            ts: start,
            open: 1.0,
            high: 1.1,
            low: 0.9,
            close: 1.05,
            volume: 10.0,
        };
        let _ = agg.on_m1_event(&BarEvent::BarUpdate(tick.clone()));
        tick.close = 1.08;
        tick.high = 1.12;
        tick.volume = 12.0;
        let _ = agg.on_m1_event(&BarEvent::BarUpdate(tick.clone()));
        let _ = agg.on_m1_event(&BarEvent::BarClosed(tick));

        let rolling = agg.current(Timeframe::M5).expect("rolling 5m");
        assert_eq!(rolling.volume, 12.0);
        assert_eq!(rolling.close, 1.08);
        assert_eq!(rolling.high, 1.12);
    }

    #[test]
    fn unsorted_input_is_sorted_defensively() {
        let w = Timeframe::H4.boundary_align(1_700_000_000_000);
        let bars = vec![
            h1("DXY", w + 7_200_000, 100.8, 101.2, 100.7, 101.0, 8.0),
            h1("DXY", w, 100.0, 100.5, 99.8, 100.2, 10.0),
            h1("DXY", w + 10_800_000, 101.0, 101.1, 100.9, 100.95, 9.0),
            h1("DXY", w + 3_600_000, 100.2, 101.0, 100.1, 100.8, 12.0),
        ];
        let now = w + 4 * 3_600_000;
        let out = aggregate_h4_from_h1(&bars, "DXY", now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].open, 100.0);
        assert_eq!(out[0].close, 100.95);
    }

    #[test]
    fn crypto_pair_is_not_classified_as_weekday_only_forex() {
        assert!(!is_forex_like("COINBASE:BTCUSD"));
        assert!(is_forex_like("OANDA:EURUSD"));
        assert!(is_forex_like("TVC:DXY"));
    }

    #[test]
    fn fx_full_holidays_are_not_expected_but_adjacent_business_days_are() {
        use chrono::{TimeZone, Utc};

        // Noon New York in winter is 17:00 UTC. Keep the assertion away from
        // daily boundaries so it verifies the calendar rule itself.
        let christmas = Utc
            .with_ymd_and_hms(2025, 12, 25, 17, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let new_year = Utc
            .with_ymd_and_hms(2026, 1, 1, 17, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let christmas_eve = Utc
            .with_ymd_and_hms(2025, 12, 24, 17, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();

        assert!(!market_is_open("OANDA:EURUSD", christmas));
        assert!(!market_is_open("TVC:DXY", new_year));
        assert!(market_is_open("OANDA:EURUSD", christmas_eve));
        assert!(market_is_open("COINBASE:BTCUSD", christmas));
    }
}
