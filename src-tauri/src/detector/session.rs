use chrono::{Datelike, NaiveDate, TimeZone, Timelike};
use chrono_tz::America::New_York;

use crate::types::{Bar, Timeframe};

use super::types::{
    structure_id, IctStructure, KillZoneWindow, SessionKind, SessionRange, StructureEvent,
};
use super::{Detector, DetectorCtx};

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub enabled: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionWindow {
    session: SessionKind,
    ts_start: i64,
    ts_end: i64,
}

pub struct SessionDetector {
    symbol: String,
    cfg: SessionConfig,
    active: Option<SessionRange>,
}

impl SessionDetector {
    pub fn new(symbol: impl Into<String>, cfg: SessionConfig) -> Self {
        Self {
            symbol: symbol.into(),
            cfg,
            active: None,
        }
    }

    fn window_for_ts(ts_ms: i64) -> Option<SessionWindow> {
        let local = chrono::Utc
            .timestamp_millis_opt(ts_ms)
            .single()?
            .with_timezone(&New_York);
        let date = local.date_naive();
        let minute = local.hour() * 60 + local.minute();
        let (session, start_date, start_min, end_date, end_min) = if minute >= 20 * 60 {
            (
                SessionKind::Asia,
                date,
                20 * 60,
                date + chrono::Duration::days(1),
                0,
            )
        } else if minute < 2 * 60 {
            return None;
        } else if minute < 5 * 60 {
            (SessionKind::LondonOpen, date, 2 * 60, date, 5 * 60)
        } else if minute < 7 * 60 {
            return None;
        } else if minute < 10 * 60 {
            (SessionKind::NewYorkOpen, date, 7 * 60, date, 10 * 60)
        } else if minute < 12 * 60 {
            (SessionKind::LondonClose, date, 10 * 60, date, 12 * 60)
        } else {
            return None;
        };
        Some(SessionWindow {
            session,
            ts_start: local_ts(start_date, start_min),
            ts_end: local_ts(end_date, end_min),
        })
    }

    fn build_window(&self, window: SessionWindow) -> KillZoneWindow {
        KillZoneWindow {
            id: structure_id(&[
                &self.symbol,
                "kill_zone_window",
                window.session.tag(),
                &window.ts_start.to_string(),
            ]),
            symbol: self.symbol.clone(),
            tf: Timeframe::M1,
            session: window.session,
            label: window.session.label().to_string(),
            ts_start: window.ts_start,
            ts_end: window.ts_end,
        }
    }

    fn build_range(&self, window: SessionWindow, bar: &Bar) -> SessionRange {
        SessionRange {
            id: structure_id(&[
                &self.symbol,
                "session_range",
                window.session.tag(),
                &window.ts_start.to_string(),
            ]),
            symbol: self.symbol.clone(),
            tf: Timeframe::M1,
            session: window.session,
            label: window.session.label().to_string(),
            ts_start: window.ts_start,
            ts_end: window.ts_end,
            high: bar.high,
            low: bar.low,
            high_ts: bar.ts,
            low_ts: bar.ts,
            finalized: false,
        }
    }

    fn step_active(
        &mut self,
        bar: &Bar,
        current_window: Option<SessionWindow>,
    ) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        let Some(mut range) = self.active.take() else {
            return out;
        };
        if current_window
            .is_some_and(|w| w.ts_start == range.ts_start && w.session == range.session)
        {
            let mut changed = false;
            if bar.high > range.high {
                range.high = bar.high;
                range.high_ts = bar.ts;
                changed = true;
            }
            if bar.low < range.low {
                range.low = bar.low;
                range.low_ts = bar.ts;
                changed = true;
            }
            if changed {
                out.push(StructureEvent::Update(IctStructure::SessionRange(
                    range.clone(),
                )));
            }
            self.active = Some(range);
            return out;
        }
        if !range.finalized {
            range.finalized = true;
            out.push(StructureEvent::Update(IctStructure::SessionRange(range)));
        }
        out
    }
}

impl Detector for SessionDetector {
    fn name(&self) -> &'static str {
        "session"
    }

    fn on_closed(&mut self, bars: &[Bar], _ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        let Some(bar) = bars.last() else {
            return Vec::new();
        };
        if bar.tf != Timeframe::M1 {
            return Vec::new();
        }
        let current_window = Self::window_for_ts(bar.ts);
        let mut out = self.step_active(bar, current_window);
        let Some(window) = current_window else {
            return out;
        };
        let already_active = self.active.as_ref().is_some_and(|range| {
            range.ts_start == window.ts_start && range.session == window.session
        });
        if already_active {
            return out;
        }
        let kz = self.build_window(window);
        let range = self.build_range(window, bar);
        self.active = Some(range.clone());
        out.push(StructureEvent::New(IctStructure::KillZoneWindow(kz)));
        out.push(StructureEvent::New(IctStructure::SessionRange(range)));
        out
    }

    fn reset(&mut self) {
        self.active = None;
    }
}

fn local_ts(date: NaiveDate, minute: u32) -> i64 {
    let hour = minute / 60;
    let min = minute % 60;
    New_York
        .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, min, 0)
        .single()
        .or_else(|| {
            New_York
                .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, min, 0)
                .earliest()
        })
        .expect("session local time resolves")
        .with_timezone(&chrono::Utc)
        .timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::swing::SwingSeries;

    fn ts(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        New_York
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .or_else(|| New_York.with_ymd_and_hms(y, m, d, h, min, 0).earliest())
            .unwrap()
            .with_timezone(&chrono::Utc)
            .timestamp_millis()
    }

    fn b(ts: i64, h: f64, l: f64) -> Bar {
        Bar {
            symbol: "EURUSD".into(),
            tf: Timeframe::M1,
            ts,
            open: (h + l) / 2.0,
            high: h,
            low: l,
            close: (h + l) / 2.0,
            volume: 0.0,
        }
    }

    fn detector() -> SessionDetector {
        SessionDetector::new("EURUSD", SessionConfig::default())
    }

    #[test]
    fn asia_window_crosses_calendar_day() {
        let w = SessionDetector::window_for_ts(ts(2026, 6, 24, 20, 0)).unwrap();
        assert_eq!(w.session, SessionKind::Asia);
        assert_eq!(w.ts_start, ts(2026, 6, 24, 20, 0));
        assert_eq!(w.ts_end, ts(2026, 6, 25, 0, 0));
        assert!(SessionDetector::window_for_ts(ts(2026, 6, 25, 0, 0)).is_none());
    }

    #[test]
    fn london_ny_and_close_windows_generate() {
        assert_eq!(
            SessionDetector::window_for_ts(ts(2026, 6, 25, 2, 30))
                .unwrap()
                .session,
            SessionKind::LondonOpen
        );
        assert_eq!(
            SessionDetector::window_for_ts(ts(2026, 6, 25, 7, 30))
                .unwrap()
                .session,
            SessionKind::NewYorkOpen
        );
        assert_eq!(
            SessionDetector::window_for_ts(ts(2026, 6, 25, 10, 30))
                .unwrap()
                .session,
            SessionKind::LondonClose
        );
    }

    #[test]
    fn dst_dates_still_use_ny_local_clock() {
        let spring = SessionDetector::window_for_ts(ts(2026, 3, 8, 20, 5)).unwrap();
        assert_eq!(spring.ts_start, ts(2026, 3, 8, 20, 0));
        assert_eq!(spring.ts_end, ts(2026, 3, 9, 0, 0));
        let fall = SessionDetector::window_for_ts(ts(2026, 11, 1, 20, 5)).unwrap();
        assert_eq!(fall.ts_start, ts(2026, 11, 1, 20, 0));
        assert_eq!(fall.ts_end, ts(2026, 11, 2, 0, 0));
    }

    #[test]
    fn bars_update_high_low_inside_session() {
        let swings = SwingSeries::default();
        let mut det = detector();
        let evs = det.on_closed(
            &[b(ts(2026, 6, 24, 20, 0), 1.10, 1.09)],
            &DetectorCtx::new(&swings),
        );
        assert!(evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::KillZoneWindow(_)))));
        assert!(evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::SessionRange(_)))));
        let evs = det.on_closed(
            &[
                b(ts(2026, 6, 24, 20, 0), 1.10, 1.09),
                b(ts(2026, 6, 24, 20, 1), 1.12, 1.08),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(|ev| matches!(ev, StructureEvent::Update(IctStructure::SessionRange(r)) if r.high == 1.12 && r.low == 1.08)));
    }

    #[test]
    fn session_finalizes_after_end() {
        let swings = SwingSeries::default();
        let mut det = detector();
        det.on_closed(
            &[b(ts(2026, 6, 24, 23, 59), 1.10, 1.09)],
            &DetectorCtx::new(&swings),
        );
        let evs = det.on_closed(
            &[
                b(ts(2026, 6, 24, 23, 59), 1.10, 1.09),
                b(ts(2026, 6, 25, 0, 0), 1.11, 1.10),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(
            |ev| matches!(ev, StructureEvent::Update(IctStructure::SessionRange(r)) if r.finalized)
        ));
    }

    #[test]
    fn ids_are_stable_for_same_window() {
        let det = detector();
        let bar = b(ts(2026, 6, 24, 20, 0), 1.10, 1.09);
        let w = SessionDetector::window_for_ts(bar.ts).unwrap();
        let a = det.build_range(w, &bar);
        let b = det.build_range(w, &bar);
        assert_eq!(a.id, b.id);
        assert_eq!(det.build_window(w).id, det.build_window(w).id);
    }
}
