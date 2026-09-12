//! KillZone scheduler — §5.2.7.
//!
//! Independent tokio task (NOT a Detector). On boot we emit today + the
//! next day's NY-local windows so the chart has ~30h of forward coverage,
//! then loop: sleep until the next NY 00:00 → emit the day-after-next's
//! windows → repeat. This keeps the ~30h horizon rolling without ever
//! returning, so a process running >24h won't run out of KZ data.
//!
//! Windows (NY-local, per ICT 2022 conventions used in the doc):
//!   - AKZ  : 20:00 prev day .. 00:00          (Asia)
//!   - LOKZ : 02:00 .. 05:00                   (London Open)
//!   - NYOKZ: 07:00 .. 10:00                   (NY Open)
//!   - LCKZ : 10:00 .. 12:00                   (London Close)
//!   - SB-AM: 03:00 .. 04:00 (Silver Bullet)
//!   - SB-LO: 10:00 .. 11:00
//!   - SB-PM: 14:00 .. 15:00

use chrono::{Datelike, NaiveDate, TimeZone};
use chrono_tz::America::New_York;
use tokio::sync::broadcast;

use super::types::{structure_id, IctStructure, KillZoneKind, KillZoneSpan, StructureEvent};
use crate::types::Timeframe;

const KIND_TF: Timeframe = Timeframe::M1; // arbitrary anchor; UI ignores tf for KZ

/// How many days ahead we keep KZ windows pre-emitted at any given time.
/// 2 = today + tomorrow → ~30h horizon at worst.
const HORIZON_DAYS: i64 = 2;

/// How many days back we backfill at boot so historical bars on the chart
/// also have visible KZ tints. Required when the chart is showing a
/// market-closed window (weekends) and the rolling-edge bar predates the
/// boot day's KZ windows.
const HISTORY_DAYS: i64 = 7;

#[derive(Clone, Copy)]
struct Window {
    kind: KillZoneKind,
    start_h: u32,
    start_m: u32,
    end_h: u32,
    end_m: u32,
    /// True when the window starts the previous calendar day.
    prev_day_start: bool,
}

const WINDOWS: &[Window] = &[
    Window {
        kind: KillZoneKind::Asia,
        start_h: 20,
        start_m: 0,
        end_h: 0,
        end_m: 0,
        prev_day_start: true,
    },
    Window {
        kind: KillZoneKind::LondonOpen,
        start_h: 2,
        start_m: 0,
        end_h: 5,
        end_m: 0,
        prev_day_start: false,
    },
    Window {
        kind: KillZoneKind::NewYorkOpen,
        start_h: 7,
        start_m: 0,
        end_h: 10,
        end_m: 0,
        prev_day_start: false,
    },
    Window {
        kind: KillZoneKind::LondonClose,
        start_h: 10,
        start_m: 0,
        end_h: 12,
        end_m: 0,
        prev_day_start: false,
    },
    Window {
        kind: KillZoneKind::SilverBulletAsia,
        start_h: 3,
        start_m: 0,
        end_h: 4,
        end_m: 0,
        prev_day_start: false,
    },
    Window {
        kind: KillZoneKind::SilverBulletLondon,
        start_h: 10,
        start_m: 0,
        end_h: 11,
        end_m: 0,
        prev_day_start: false,
    },
    Window {
        kind: KillZoneKind::SilverBulletNewYork,
        start_h: 14,
        start_m: 0,
        end_h: 15,
        end_m: 0,
        prev_day_start: false,
    },
];

pub struct KillZoneScheduler {
    pub symbol: String,
    pub event_tx: broadcast::Sender<StructureEvent>,
}

impl KillZoneScheduler {
    pub fn new(symbol: impl Into<String>, event_tx: broadcast::Sender<StructureEvent>) -> Self {
        Self {
            symbol: symbol.into(),
            event_tx,
        }
    }

    /// Long-running task: emit today + tomorrow at boot, then on every
    /// NY 00:00 emit the new "day-after-next" so the rolling horizon stays
    /// at HORIZON_DAYS days ahead. Receivers de-dup by `id` (blake3 of
    /// symbol|kind|ts_start) so re-emitting the same window is harmless.
    pub fn spawn(self) {
        tokio::spawn(async move {
            let now_ny = chrono::Utc::now().with_timezone(&New_York);
            let today = now_ny.date_naive();
            tracing::info!(
                symbol = %self.symbol,
                today = %today,
                horizon_days = HORIZON_DAYS,
                history_days = HISTORY_DAYS,
                receivers = self.event_tx.receiver_count(),
                "killzone scheduler boot fan-out starting"
            );
            // Initial fan-out covering [today - HISTORY_DAYS, today + HORIZON_DAYS).
            let mut emitted = 0usize;
            for offset in -HISTORY_DAYS..HORIZON_DAYS {
                let date = today + chrono::Duration::days(offset);
                for w in WINDOWS {
                    if let Some(span) = build_span(&self.symbol, date, *w) {
                        let label = span.label.clone();
                        let ts_start = span.ts_start;
                        match self
                            .event_tx
                            .send(StructureEvent::New(IctStructure::KillZone(span)))
                        {
                            Ok(_) => emitted += 1,
                            Err(e) => tracing::warn!(
                                error = ?e, %label, ts_start,
                                "killzone send dropped"
                            ),
                        }
                    }
                }
            }
            tracing::info!(emitted, "killzone scheduler boot fan-out done");

            // Steady state: every time we cross NY 00:00 emit the new
            // (today + HORIZON_DAYS - 1) day's windows.
            let mut last_emitted_far = today + chrono::Duration::days(HORIZON_DAYS - 1);
            loop {
                let now_ny = chrono::Utc::now().with_timezone(&New_York);
                let next_midnight = next_ny_midnight(now_ny);
                let sleep_for = (next_midnight - chrono::Utc::now())
                    .to_std()
                    .unwrap_or(std::time::Duration::from_secs(60));
                tokio::time::sleep(sleep_for).await;

                let today_ny = chrono::Utc::now().with_timezone(&New_York).date_naive();
                let target = today_ny + chrono::Duration::days(HORIZON_DAYS - 1);
                if target <= last_emitted_far {
                    // Clock skew / spurious wake — skip until we actually advanced.
                    continue;
                }
                for w in WINDOWS {
                    if let Some(span) = build_span(&self.symbol, target, *w) {
                        let _ = self
                            .event_tx
                            .send(StructureEvent::New(IctStructure::KillZone(span)));
                    }
                }
                last_emitted_far = target;
            }
        });
    }
}

/// Return the next NY-local midnight strictly after `now_ny`, expressed
/// as a UTC instant. Handles DST transitions by walking the local
/// calendar day boundary, not by adding 24h.
fn next_ny_midnight(now_ny: chrono::DateTime<chrono_tz::Tz>) -> chrono::DateTime<chrono::Utc> {
    let date = now_ny.date_naive() + chrono::Duration::days(1);
    let local = New_York
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        // DST spring-forward in the US doesn't affect midnight, but pick the
        // earliest valid instant defensively.
        .or_else(|| {
            New_York
                .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
                .earliest()
        })
        .expect("midnight resolves");
    local.with_timezone(&chrono::Utc)
}

fn build_span(symbol: &str, date: NaiveDate, w: Window) -> Option<KillZoneSpan> {
    let start_date = if w.prev_day_start {
        date - chrono::Duration::days(1)
    } else {
        date
    };
    let start_local = New_York
        .with_ymd_and_hms(
            start_date.year(),
            start_date.month(),
            start_date.day(),
            w.start_h,
            w.start_m,
            0,
        )
        .single()?;
    let end_local = New_York
        .with_ymd_and_hms(date.year(), date.month(), date.day(), w.end_h, w.end_m, 0)
        .single()?;
    let ts_start = start_local.timestamp_millis();
    let ts_end = end_local.timestamp_millis();
    let id = structure_id(&[symbol, "kz", w.kind.label(), &ts_start.to_string()]);
    Some(KillZoneSpan {
        id,
        symbol: symbol.into(),
        tf: KIND_TF,
        kind: w.kind,
        label: w.kind.label().into(),
        ts_start,
        ts_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    /// Generic sanity: all 7 windows materialize for an arbitrary date.
    #[test]
    fn build_span_emits_seven_windows_per_day() {
        let date = NaiveDate::from_ymd_opt(2024, 6, 17).unwrap();
        let mut spans: Vec<KillZoneSpan> = Vec::new();
        for w in WINDOWS {
            if let Some(s) = build_span("EURUSD", date, *w) {
                spans.push(s);
            }
        }
        assert_eq!(spans.len(), 7, "expected 7 windows, got {}", spans.len());
        for s in &spans {
            assert!(s.ts_end > s.ts_start, "{} end must be > start", s.label);
        }
    }

    /// Spring-forward DST day (US): 2024-03-10 has only 23 local hours.
    /// next_ny_midnight should still hand back the next NY-local 00:00,
    /// expressed in UTC, by walking the calendar — not by adding 24h.
    #[test]
    fn next_ny_midnight_handles_spring_forward() {
        let now = New_York.with_ymd_and_hms(2024, 3, 10, 12, 0, 0).unwrap();
        let next = next_ny_midnight(now);
        let next_ny = next.with_timezone(&New_York);
        assert_eq!(
            next_ny.date_naive(),
            NaiveDate::from_ymd_opt(2024, 3, 11).unwrap()
        );
        assert_eq!(next_ny.hour(), 0);
        assert_eq!(next_ny.minute(), 0);
    }

    /// Fall-back DST day (US): 2024-11-03 has 25 local hours.
    #[test]
    fn next_ny_midnight_handles_fall_back() {
        let now = New_York.with_ymd_and_hms(2024, 11, 3, 12, 0, 0).unwrap();
        let next = next_ny_midnight(now);
        let next_ny = next.with_timezone(&New_York);
        assert_eq!(
            next_ny.date_naive(),
            NaiveDate::from_ymd_opt(2024, 11, 4).unwrap()
        );
        assert_eq!(next_ny.hour(), 0);
    }
}
