//! Common domain types shared across modules.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::America::New_York;
use serde::{Deserialize, Serialize};

/// Supported chart timeframes. Only the strings used on the wire matter for M1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Timeframe {
    M1,
    M5,
    M15,
    M30,
    H1,
    H4,
    D1,
    W1,
    MN1,
}

impl serde::Serialize for Timeframe {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.tag())
    }
}

impl<'de> serde::Deserialize<'de> for Timeframe {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(deserializer)?;
        Timeframe::from_tag(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown tf tag: {}", s)))
    }
}

impl Timeframe {
    /// String the TradingView WS protocol expects in `create_series`.
    pub fn tv_resolution(self) -> &'static str {
        match self {
            Timeframe::M1 => "1",
            Timeframe::M5 => "5",
            Timeframe::M15 => "15",
            Timeframe::M30 => "30",
            Timeframe::H1 => "60",
            Timeframe::H4 => "240",
            Timeframe::D1 => "1D",
            Timeframe::W1 => "1W",
            Timeframe::MN1 => "1M",
        }
    }

    /// Tag stored alongside bars in SQLite (stable, human readable).
    pub fn tag(self) -> &'static str {
        match self {
            Timeframe::M1 => "1m",
            Timeframe::M5 => "5m",
            Timeframe::M15 => "15m",
            Timeframe::M30 => "30m",
            Timeframe::H1 => "1h",
            Timeframe::H4 => "4h",
            Timeframe::D1 => "1d",
            Timeframe::W1 => "1w",
            Timeframe::MN1 => "1mo",
        }
    }

    /// Parse a tf tag back into the enum (used by IPC commands).
    pub fn from_tag(s: &str) -> Option<Self> {
        Some(match s {
            "1m" => Timeframe::M1,
            "5m" => Timeframe::M5,
            "15m" => Timeframe::M15,
            "30m" => Timeframe::M30,
            "1h" => Timeframe::H1,
            "4h" => Timeframe::H4,
            "1d" => Timeframe::D1,
            "1w" => Timeframe::W1,
            "1mo" => Timeframe::MN1,
            _ => return None,
        })
    }

    /// Length of a bar in milliseconds. Used to detect bar-closed transitions.
    /// For non-fixed periods (D1/W1/MN1) returns the *nominal* span, but never
    /// rely on this for boundary detection — use `boundary_align` instead.
    pub fn duration_ms(self) -> i64 {
        match self {
            Timeframe::M1 => 60_000,
            Timeframe::M5 => 5 * 60_000,
            Timeframe::M15 => 15 * 60_000,
            Timeframe::M30 => 30 * 60_000,
            Timeframe::H1 => 60 * 60_000,
            Timeframe::H4 => 4 * 60 * 60_000,
            Timeframe::D1 => 24 * 60 * 60_000,
            Timeframe::W1 => 7 * 24 * 60 * 60_000,
            Timeframe::MN1 => 30 * 24 * 60 * 60_000,
        }
    }

    /// All higher TFs that 1m bars roll up into (in ascending order).
    pub fn higher_than_m1() -> &'static [Timeframe] {
        &[
            Timeframe::M5,
            Timeframe::M15,
            Timeframe::M30,
            Timeframe::H1,
            Timeframe::H4,
            Timeframe::D1,
            Timeframe::W1,
            Timeframe::MN1,
        ]
    }

    /// Floor a millisecond timestamp to the start of the bar this TF would
    /// place it in, following ICT/IPDA convention:
    ///
    /// * 5m / 15m / 1h: UTC natural boundary
    /// * 4h: NY-local 23/03/07/11/15/19 (DST-aware, matches TradingView chart 4h)
    /// * 1d: NY-local 17:00 cuts the day
    /// * 1w: Sunday 17:00 NY-local opens the week
    /// * 1mo: NY-local first calendar day at 17:00 of the previous month
    /// * 1m: trivial UTC minute boundary
    pub fn boundary_align(self, ts_ms: i64) -> i64 {
        match self {
            Timeframe::M1 => floor_utc(ts_ms, 60_000),
            Timeframe::M5 => floor_utc(ts_ms, 5 * 60_000),
            Timeframe::M15 => floor_utc(ts_ms, 15 * 60_000),
            Timeframe::M30 => floor_utc(ts_ms, 30 * 60_000),
            Timeframe::H1 => floor_utc(ts_ms, 60 * 60_000),
            Timeframe::H4 => floor_h4_ny(ts_ms),
            Timeframe::D1 => floor_d_ny(ts_ms),
            Timeframe::W1 => floor_w_ny(ts_ms),
            Timeframe::MN1 => floor_mn_ny(ts_ms),
        }
    }
}

fn floor_utc(ts_ms: i64, period_ms: i64) -> i64 {
    ts_ms - ts_ms.rem_euclid(period_ms)
}

/// 4h bars aligned to 23/03/07/11/15/19 NY-local (matches TradingView chart
/// 4h grid; TV WebSocket API uses a different 01/05/09/13/17/21 grid, so we
/// re-aggregate from 1h instead of using TV-raw 4h). DST handled via civil time.
fn floor_h4_ny(ts_ms: i64) -> i64 {
    let utc: DateTime<Utc> = Utc.timestamp_millis_opt(ts_ms).single().expect("ts");
    let ny = utc.with_timezone(&New_York);
    // Floor in **civil time**, then resolve back to UTC. Doing the subtraction
    // on the NY-local DateTime would cross DST jumps and shift by an extra
    // hour around spring-forward.
    // Anchors at NY-local 23/03/07/11/15/19. For hours 0-2, floor goes back
    // to 23 of the previous calendar day.
    let h = ny.hour() as i64;
    let anchor = ((h - 23).rem_euclid(4)) as i64;
    let floored = h - anchor;
    let (y, m, d, hh) = if floored < 0 {
        let prev = ny.date_naive() - chrono::Duration::days(1);
        (prev.year(), prev.month(), prev.day(), (floored + 24) as u32)
    } else {
        (ny.year(), ny.month(), ny.day(), floored as u32)
    };
    let local = New_York
        .with_ymd_and_hms(y, m, d, hh, 0, 0)
        .single()
        .or_else(|| {
            // Pick earliest valid local time on ambiguous instants.
            New_York.with_ymd_and_hms(y, m, d, hh, 0, 0).earliest()
        })
        .expect("h4 floor resolves");
    local.with_timezone(&Utc).timestamp_millis()
}

/// "NY day" bar. ICT IPDA day starts at 17:00 NY-local, so subtract 17h, take
/// the local calendar date, then add 17:00 back.
fn floor_d_ny(ts_ms: i64) -> i64 {
    let utc: DateTime<Utc> = Utc.timestamp_millis_opt(ts_ms).single().expect("ts");
    let ny = utc.with_timezone(&New_York);
    let shifted = ny - chrono::Duration::hours(17);
    let date = shifted.date_naive();
    day_open_ny(date)
}

/// Weekly bar. Week open = Sunday 17:00 NY-local (= "next IPDA day after Friday").
fn floor_w_ny(ts_ms: i64) -> i64 {
    let utc: DateTime<Utc> = Utc.timestamp_millis_opt(ts_ms).single().expect("ts");
    let ny = utc.with_timezone(&New_York);
    let shifted = ny - chrono::Duration::hours(17);
    let date = shifted.date_naive();
    // chrono Weekday: Sun=Sun. number_from_sunday(): Sun=1.
    let dow_from_sun = date.weekday().num_days_from_sunday() as i64;
    let sunday = date - chrono::Duration::days(dow_from_sun);
    day_open_ny(sunday)
}

/// Monthly bar. First-of-month at 17:00 NY-local of *previous* day's IPDA day,
/// i.e. day shift by 17h, take the calendar month-start.
fn floor_mn_ny(ts_ms: i64) -> i64 {
    let utc: DateTime<Utc> = Utc.timestamp_millis_opt(ts_ms).single().expect("ts");
    let ny = utc.with_timezone(&New_York);
    let shifted = ny - chrono::Duration::hours(17);
    let date = shifted.date_naive();
    let first = NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap();
    day_open_ny(first)
}

/// 17:00 NY-local on the given calendar date, rendered as UTC ms.
fn day_open_ny(date: NaiveDate) -> i64 {
    // Use single() and gracefully fall back if the wall clock 17:00 doesn't
    // exist (DST spring-forward never affects 17:00) or is ambiguous.
    let local = New_York
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 17, 0, 0)
        .single()
        .expect("17:00 NY exists");
    local.with_timezone(&Utc).timestamp_millis()
}

/// OHLCV bar, ts is **milliseconds since UNIX epoch**.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bar {
    pub symbol: String,
    pub tf: Timeframe,
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// Events emitted by any `MarketDataProvider` implementation.
#[derive(Clone, Debug)]
pub enum BarEvent {
    /// Initial historical backfill (newest last).
    Historical(Vec<Bar>),
    /// In-progress (un-closed) bar updated.
    BarUpdate(Bar),
    /// Bar finalized — safe to persist and feed to the ICT engine.
    BarClosed(Bar),
}
