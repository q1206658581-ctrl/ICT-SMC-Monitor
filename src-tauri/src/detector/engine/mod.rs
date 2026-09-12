//! IctEngine — orchestrates per-(symbol, tf) detectors and broadcasts events.

mod confluence;
mod handle;
mod params;
mod registry;
mod replay;
mod runtime;
mod state;

pub use handle::EngineHandle;
pub use state::{DetectorToggles, IctEngine, DETECTOR_HISTORY};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::fvg::{FvgConfig, FvgDetector};
    use crate::detector::opening_gap::{OpeningGapConfig, OpeningGapDetector};
    use crate::detector::ote::{OteConfig, OteDetector};
    use crate::detector::premium_discount::{PremiumDiscountConfig, PremiumDiscountDetector};
    use crate::detector::session::{SessionConfig, SessionDetector};
    use crate::detector::types::{
        Direction, GapState, IctStructure, LevelMarker, LiquidityPoolKind, LiquiditySide,
        LiquiditySweep, Mss, ObState, OrderBlock, PdSide, Po3ContextKind, Po3Stage, Po3StageBox,
        Po3State, PowerOf3, ReversalConfirmKind, StructureEvent,
    };
    use crate::types::{Bar, Timeframe};
    use serde_json::json;
    use tokio::sync::broadcast;

    fn b(ts: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 0.0,
        }
    }

    /// Sanity: apply_detector_param hits the registered detector, resets it,
    /// then replays history and re-emits structures with the new param.
    #[test]
    fn apply_param_resets_and_replays() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(FvgDetector::new(
                "EURUSD",
                Timeframe::M5,
                FvgConfig {
                    min_size_pips: 0.0,
                    pip_size: 0.0001,
                },
            )),
        );

        // Three bars forming a bullish FVG with gap = 0.0010 (= 10 pips on
        // EURUSD-style pip_size).
        let bars = [
            b(60_000, 1.0000, 1.0010, 0.9995, 1.0008),
            b(120_000, 1.0008, 1.0030, 1.0008, 1.0028),
            b(180_000, 1.0028, 1.0050, 1.0020, 1.0045),
        ];
        for bar in &bars {
            eng.on_closed_bar(bar);
        }
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG should be tracked"
        );

        // Tighten min_size_pips to 50 → existing 10-pip FVG should disappear.
        let n = eng.apply_detector_param("fvg", "min_size_pips", &json!(50.0));
        assert_eq!(
            n, 1,
            "exactly one FvgDetector instance should accept the param"
        );
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            0,
            "FVG should be filtered out after replay"
        );

        // Drop threshold back; replay should re-emit.
        let n = eng.apply_detector_param("fvg", "min_size_pips", &json!(0.0));
        assert_eq!(n, 1);
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG should reappear after relaxing the filter"
        );
    }

    #[test]
    fn seeding_tracks_only_changed_structures_for_incremental_persist() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(FvgDetector::new(
                "EURUSD",
                Timeframe::M5,
                FvgConfig {
                    min_size_pips: 0.0,
                    pip_size: 0.0001,
                },
            )),
        );
        eng.seeding
            .store(true, std::sync::atomic::Ordering::Relaxed);
        for bar in [
            b(60_000, 1.0000, 1.0010, 0.9995, 1.0008),
            b(120_000, 1.0008, 1.0030, 1.0008, 1.0028),
            b(180_000, 1.0028, 1.0050, 1.0020, 1.0045),
        ] {
            eng.seed_closed_bar(&bar);
        }

        let dirty = eng.take_seed_dirty_structures();
        assert_eq!(dirty.len(), 1);
        assert!(matches!(dirty[0], IctStructure::Fvg(_)));
        assert!(eng.take_seed_dirty_structures().is_empty());
    }

    #[test]
    fn mss_fractal_param_updates_shared_swing_series() {
        use crate::detector::mss::{MssConfig, MssDetector};
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(MssDetector::new(
                "EURUSD",
                Timeframe::M5,
                MssConfig::default(),
            )),
        );
        assert_eq!(eng.swing_fractal("EURUSD", Timeframe::M5), Some(2));
        assert_eq!(eng.apply_detector_param("mss", "fractal_n", &json!(4)), 1);
        assert_eq!(eng.swing_fractal("EURUSD", Timeframe::M5), Some(4));
    }

    /// M4a/M4b UI semantics: total detector switches are display-only.
    /// They must not invalidate engine structures; parameter changes are
    /// the path that resets/replays detector state.
    #[test]
    fn display_toggle_does_not_clear_structures() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(FvgDetector::new(
                "EURUSD",
                Timeframe::M5,
                FvgConfig::default(),
            )),
        );
        // Bullish FVG: bar[0].high < bar[2].low.
        eng.on_closed_bar(&b(0, 1.1000, 1.1010, 1.0990, 1.1005));
        eng.on_closed_bar(&b(60_000, 1.1015, 1.1030, 1.1010, 1.1025));
        eng.on_closed_bar(&b(120_000, 1.1030, 1.1050, 1.1020, 1.1045));
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG should be present initially"
        );

        eng.set_detector_enabled("fvg", false);
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG should remain cached after display-only disable"
        );
        assert!(
            eng.toggles.is_enabled("fvg"),
            "display-only disable must not pause backend calculation"
        );

        eng.set_detector_enabled("fvg", true);
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG should still be immediately available after re-enable"
        );
        assert!(eng.toggles.is_enabled("fvg"));
    }

    #[test]
    fn ote_fib_param_replays_existing_zone() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.set_swing_fractal("EURUSD", Timeframe::M5, 1);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(OteDetector::new(
                "EURUSD",
                Timeframe::M5,
                OteConfig::default(),
            )),
        );
        let bars = [
            b(0, 1.0, 1.0, 1.0, 1.0),
            b(60_000, 1.15, 1.2, 1.1, 1.15),
            b(120_000, 1.08, 1.1, 1.05, 1.08),
            b(180_000, 1.28, 1.3, 1.2, 1.28),
            b(240_000, 1.15, 1.2, 1.1, 1.15),
        ];
        for bar in &bars {
            eng.on_closed_bar(bar);
        }
        // Collect *all* OTE ids/params before the param change.
        let before: Vec<(String, f64)> = eng
            .list_active("EURUSD", Timeframe::M5)
            .into_iter()
            .filter_map(|s| match s {
                IctStructure::Ote(o) => Some((o.id, o.fib_low)),
                _ => None,
            })
            .collect();
        assert!(!before.is_empty(), "initial OTE set should not be empty");
        assert!(
            before.iter().all(|(_, fl)| (fl - 0.62).abs() < 1e-9),
            "all before-OTEs should use default fib_low 0.62"
        );

        assert_eq!(eng.apply_detector_param("ote", "fib_low", &json!(0.50)), 1);

        // After replay every OTE must carry the new fib_low and have a
        // fresh id (fib_low is part of the id hash).
        let after: Vec<(String, f64)> = eng
            .list_active("EURUSD", Timeframe::M5)
            .into_iter()
            .filter_map(|s| match s {
                IctStructure::Ote(o) => Some((o.id, o.fib_low)),
                _ => None,
            })
            .collect();
        assert!(!after.is_empty(), "replayed OTE set should not be empty");
        assert!(
            after.iter().all(|(_, fl)| (fl - 0.50).abs() < 1e-9),
            "all after-OTEs should use fib_low 0.50"
        );
        let before_ids: std::collections::HashSet<&str> =
            before.iter().map(|(id, _)| id.as_str()).collect();
        let after_ids: std::collections::HashSet<&str> =
            after.iter().map(|(id, _)| id.as_str()).collect();
        assert!(
            before_ids.is_disjoint(&after_ids),
            "replay must emit fresh ids (fib_low changed)"
        );
    }

    #[test]
    fn premium_discount_tolerance_param_replays_state() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.set_swing_fractal("EURUSD", Timeframe::M5, 1);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(PremiumDiscountDetector::new(
                "EURUSD",
                Timeframe::M5,
                PremiumDiscountConfig::default(),
            )),
        );
        let bars = [
            b(0, 1.0, 1.0, 1.0, 1.0),
            b(60_000, 1.15, 1.2, 1.1, 1.15),
            b(120_000, 1.08, 1.1, 1.05, 1.08),
            b(180_000, 1.28, 1.3, 1.2, 1.28),
            b(240_000, 1.1752, 1.2, 1.1, 1.1752),
        ];
        for bar in &bars {
            eng.on_closed_bar(bar);
        }
        let before = eng
            .list_active("EURUSD", Timeframe::M5)
            .into_iter()
            .find_map(|s| match s {
                IctStructure::PremiumDiscount(pd) => Some(pd),
                _ => None,
            })
            .expect("initial premium/discount");
        assert_eq!(before.current_side, PdSide::Premium);

        assert_eq!(
            eng.apply_detector_param(
                "premium_discount",
                "equilibrium_tolerance_pips",
                &json!(3.0)
            ),
            1
        );

        let after = eng
            .list_active("EURUSD", Timeframe::M5)
            .into_iter()
            .find_map(|s| match s {
                IctStructure::PremiumDiscount(pd) => Some(pd),
                _ => None,
            })
            .expect("replayed premium/discount");
        assert_eq!(after.current_side, PdSide::Equilibrium);
        assert_eq!(before.id, after.id);
    }

    #[test]
    fn opening_gap_param_replays_and_is_cross_tf() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M1,
            Box::new(OpeningGapDetector::new(
                "EURUSD",
                OpeningGapConfig {
                    enabled: true,
                    min_nwog_size_pips: 2.0,
                    min_ndog_size_pips: 0.5,
                    pip_size: 0.0001,
                },
            )),
        );
        let boundary = Timeframe::D1.boundary_align(1_700_000_000_000);
        let bars = [
            Bar {
                tf: Timeframe::M1,
                ..b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1000)
            },
            Bar {
                tf: Timeframe::M1,
                ..b(boundary, 1.1002, 1.1004, 1.1001, 1.1003)
            },
        ];
        for bar in &bars {
            eng.on_closed_bar(bar);
        }
        let before = eng
            .list_active("EURUSD", Timeframe::M15)
            .into_iter()
            .find_map(|s| match s {
                IctStructure::Ndog(g) => Some(g),
                _ => None,
            })
            .expect("NDOG visible cross-TF");
        assert_eq!(before.state, GapState::Active);

        assert_eq!(
            eng.apply_detector_param("opening_gap", "min_ndog_size_pips", &json!(3.0)),
            1
        );
        assert!(eng
            .list_active("EURUSD", Timeframe::M15)
            .into_iter()
            .all(|s| !matches!(s, IctStructure::Ndog(_))));
    }

    #[test]
    fn session_range_is_cross_tf() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M1,
            Box::new(SessionDetector::new("EURUSD", SessionConfig::default())),
        );
        let bar = Bar {
            tf: Timeframe::M1,
            ..b(1_782_345_600_000, 1.10, 1.12, 1.08, 1.11)
        };
        eng.on_closed_bar(&bar);
        let rows = eng.list_active("EURUSD", Timeframe::M15);
        assert!(rows
            .iter()
            .any(|s| matches!(s, IctStructure::SessionRange(_))));
        assert!(rows
            .iter()
            .any(|s| matches!(s, IctStructure::KillZoneWindow(_))));
    }

    #[test]
    fn po3_is_execution_tf_scoped() {
        let (tx, _rx) = broadcast::channel(64);
        let eng = IctEngine::new(tx);
        eng.hydrate(IctStructure::PowerOf3(PowerOf3 {
            id: "po3-1".into(),
            symbol: "EURUSD".into(),
            tf: Timeframe::M1,
            direction: Direction::Bullish,
            state: Po3State::ReversalConfirmed,
            context_kind: Po3ContextKind::HtfFvg,
            context_structure_ids: vec!["fvg-1".into()],
            context_timeframes: vec![Timeframe::M5],
            accumulation_start_ts: 1_000,
            accumulation_end_ts: 2_000,
            accumulation_high: 1.2,
            accumulation_low: 1.1,
            sweep_ts: 2_500,
            sweep_price: 1.09,
            confirm_ts: 3_000,
            confirm_id: "cisd-1".into(),
            confirm_kind: ReversalConfirmKind::Cisd,
            entry_ts: None,
            entry_price: None,
            entry_tf: None,
            entry_kind: None,
            entry_id: None,
            bos_id: None,
            quality_score: 4,
            stage_boxes: vec![Po3StageBox {
                stage: Po3Stage::Accumulation,
                ts_start: 1_000,
                ts_end: 2_000,
                price_low: 1.1,
                price_high: 1.2,
                label: None,
            }],
        }));
        assert!(!eng
            .list_active("EURUSD", Timeframe::M15)
            .into_iter()
            .any(|s| matches!(s, IctStructure::PowerOf3(_))));
        assert!(eng
            .list_active("EURUSD", Timeframe::M1)
            .into_iter()
            .any(|s| matches!(s, IctStructure::PowerOf3(_))));
    }

    #[test]
    fn po3_context_snapshot_includes_higher_timeframes() {
        let (tx, _rx) = broadcast::channel(64);
        let eng = IctEngine::new(tx);
        eng.hydrate(IctStructure::Fvg(crate::detector::types::Fvg {
            id: "m15-fvg".into(),
            symbol: "EURUSD".into(),
            tf: Timeframe::M15,
            direction: Direction::Bullish,
            ts_open: 1_000,
            ts_confirm: 2_000,
            price_low: 1.1000,
            price_high: 1.1050,
            state: crate::detector::types::FvgState::Active,
            ts_filled: None,
            consumed_exit_ts: None,
        }));

        let regular = eng.detector_structures_snapshot("fvg", "EURUSD", Timeframe::M5);
        assert!(!regular.iter().any(|s| matches!(s, IctStructure::Fvg(_))));

        let po3 = eng.detector_structures_snapshot("po3", "EURUSD", Timeframe::M5);
        assert!(po3
            .iter()
            .any(|s| matches!(s, IctStructure::Fvg(f) if f.tf == Timeframe::M15)));
    }

    #[test]
    fn ob_on_open_update_derives_breaker() {
        struct OpenObBreak;
        impl crate::detector::Detector for OpenObBreak {
            fn name(&self) -> &'static str {
                "open_ob_break"
            }

            fn on_closed(
                &mut self,
                _bars: &[Bar],
                _ctx: &crate::detector::DetectorCtx<'_>,
            ) -> Vec<StructureEvent> {
                Vec::new()
            }

            fn on_open(
                &mut self,
                _current: &Bar,
                _history: &[Bar],
                _ctx: &crate::detector::DetectorCtx<'_>,
            ) -> Vec<StructureEvent> {
                vec![StructureEvent::Update(IctStructure::OrderBlock(
                    OrderBlock {
                        id: "ob-open".into(),
                        symbol: "EURUSD".into(),
                        tf: Timeframe::M5,
                        direction: Direction::Bullish,
                        ts_open: 0,
                        ts_confirm: 60_000,
                        price_low: 1.0,
                        price_high: 1.1,
                        state: ObState::Mitigated,
                    },
                ))]
            }
        }

        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register("EURUSD", Timeframe::M5, Box::new(OpenObBreak));
        eng.on_closed_bar(&b(0, 1.05, 1.08, 1.02, 1.05));
        eng.on_open_bar(&b(60_000, 1.01, 1.03, 0.98, 0.99));

        assert!(eng.list_active("EURUSD", Timeframe::M5).iter().any(|s| {
            matches!(s, IctStructure::BreakerBlock(b) if b.direction == Direction::Bearish && b.source_ob_id == "ob-open")
        }));
    }

    #[test]
    fn older_historical_seed_does_not_rewind_live_stream() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(FvgDetector::new(
                "EURUSD",
                Timeframe::M5,
                FvgConfig::default(),
            )),
        );

        eng.on_closed_bar(&b(180_000, 1.1000, 1.1010, 1.0990, 1.1005));
        eng.seed_closed_bar(&b(60_000, 1.1015, 1.1030, 1.1010, 1.1025));
        eng.seed_closed_bar(&b(120_000, 1.1030, 1.1050, 1.1020, 1.1045));

        assert_eq!(
            eng.streams
                .get(&("EURUSD".to_string(), Timeframe::M5))
                .and_then(|bucket| bucket.history.back())
                .map(|bar| bar.ts),
            Some(180_000),
            "older historical bars must not rewind detector history",
        );
        assert_eq!(eng.list_active("EURUSD", Timeframe::M5).len(), 0);
    }

    #[test]
    fn liquidity_reversal_max_bars_param_rebuilds_existing_reversals() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.hydrate(IctStructure::LiquiditySweep(LiquiditySweep {
            id: "sweep1".into(),
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            side: LiquiditySide::SellSide,
            pool_kind: LiquidityPoolKind::SwingLow,
            sweep_ts: 0,
            sweep_price: 1.0,
            level_ts: -60_000,
            level_price: 1.1,
            close_price: 1.2,
        }));
        eng.hydrate(IctStructure::Mss(Mss {
            id: "mss1".into(),
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            direction: Direction::Bullish,
            break_ts: 5 * Timeframe::M5.duration_ms(),
            break_price: 1.2,
            swing_ts: 0,
            swing_price: 1.1,
        }));

        assert_eq!(
            eng.apply_detector_param("liquidity_reversal", "max_bars_after_sweep", &json!(10)),
            1
        );
        assert!(eng
            .list_active("EURUSD", Timeframe::M5)
            .iter()
            .any(|s| s.kind_tag() == "liquidity_reversal"));

        assert_eq!(
            eng.apply_detector_param("liquidity_reversal", "max_bars_after_sweep", &json!(2)),
            1
        );
        assert!(!eng
            .list_active("EURUSD", Timeframe::M5)
            .iter()
            .any(|s| s.kind_tag() == "liquidity_reversal"));
    }

    #[test]
    fn apply_param_unknown_key_returns_zero() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "X",
            Timeframe::M5,
            Box::new(FvgDetector::new("X", Timeframe::M5, FvgConfig::default())),
        );
        let n = eng.apply_detector_param("fvg", "no_such_key", &json!(1.0));
        assert_eq!(n, 0);
    }

    /// Toggling `enabled` on a display-only detector must be config-only:
    /// it should NOT wipe existing structures via reset+invalidate+replay.
    /// FVG does not handle the "enabled" key (returns 0 = not applied),
    /// so the engine skips the destructive path entirely.
    #[test]
    fn display_only_enabled_toggle_preserves_structures() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(FvgDetector::new(
                "EURUSD",
                Timeframe::M5,
                FvgConfig::default(),
            )),
        );
        // Bullish FVG: bar[0].high < bar[2].low.
        eng.on_closed_bar(&b(0, 1.1000, 1.1010, 1.0990, 1.1005));
        eng.on_closed_bar(&b(60_000, 1.1015, 1.1030, 1.1010, 1.1025));
        eng.on_closed_bar(&b(120_000, 1.1030, 1.1050, 1.1020, 1.1045));
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG should exist after initial bars"
        );

        // Toggling enabled=false must NOT destroy the FVG.
        // FVG doesn't handle "enabled" -> returns 0, but structures survive.
        assert_eq!(eng.apply_detector_param("fvg", "enabled", &json!(false)), 0);
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG must survive display-only enabled=false toggle"
        );

        // Toggling enabled=true must also NOT destroy the FVG.
        assert_eq!(eng.apply_detector_param("fvg", "enabled", &json!(true)), 0);
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG must survive display-only enabled=true toggle"
        );
    }

    /// A non-enabled param change (e.g. min_size_pips) MUST trigger the
    /// destructive reset+invalidate+replay path, re-emitting structures
    /// with the new config. This confirms Fix A only short-circuits
    /// enabled/enabled_* keys, not real config changes.
    #[test]
    fn non_enabled_param_change_triggers_replay() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(FvgDetector::new(
                "EURUSD",
                Timeframe::M5,
                FvgConfig::default(),
            )),
        );
        eng.on_closed_bar(&b(0, 1.1000, 1.1010, 1.0990, 1.1005));
        eng.on_closed_bar(&b(60_000, 1.1015, 1.1030, 1.1010, 1.1025));
        eng.on_closed_bar(&b(120_000, 1.1030, 1.1050, 1.1020, 1.1045));
        assert_eq!(eng.list_active("EURUSD", Timeframe::M5).len(), 1);

        // min_size_pips is a real config key -> returns 1, triggers replay.
        assert_eq!(
            eng.apply_detector_param("fvg", "min_size_pips", &json!(0.0)),
            1
        );
        // After replay the FVG should still be present (gap is large enough
        // to survive a 0.0 pip threshold).
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG should survive replay with min_size_pips=0"
        );
    }

    /// A detector that panics on every closed bar should not bring down
    /// other detectors registered alongside it.
    #[test]
    fn panicking_detector_is_isolated() {
        struct Boom;
        impl crate::detector::Detector for Boom {
            fn name(&self) -> &'static str {
                "boom"
            }
            fn on_closed(
                &mut self,
                _bars: &[Bar],
                _ctx: &crate::detector::DetectorCtx<'_>,
            ) -> Vec<StructureEvent> {
                panic!("intentional");
            }
        }

        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register("EURUSD", Timeframe::M5, Box::new(Boom));
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(FvgDetector::new(
                "EURUSD",
                Timeframe::M5,
                FvgConfig::default(),
            )),
        );

        // Three bars forming a bullish FVG. If isolation didn't work the
        // panic from `Boom` would unwind past `on_closed_bar` and leave
        // the FVG never produced.
        let bars = [
            b(0, 1.1000, 1.1010, 1.0990, 1.1005),
            b(60_000, 1.1015, 1.1030, 1.1010, 1.1025),
            b(120_000, 1.1030, 1.1050, 1.1020, 1.1045),
        ];
        for bar in &bars {
            eng.on_closed_bar(bar);
        }
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "FVG should be produced even though `boom` panics"
        );
    }

    /// When the next closed bar arrives more than `MAX_BAR_GAP_MULTIPLIER`
    /// TF periods after the previous bar (weekend gap), the engine
    /// resets per-TF detector working state + history so detectors do
    /// not synthesize phantom cross-gap structures. **Pre-gap structures
    /// are intentionally preserved** — they continue to age via the
    /// state machine on future bars. Mass-invalidating the bucket on a
    /// weekend gap was the bug that wiped 4h/H1 history on Monday boot.
    #[test]
    fn weekend_gap_preserves_history_and_no_phantom_fvg() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(FvgDetector::new(
                "EURUSD",
                Timeframe::M5,
                FvgConfig::default(),
            )),
        );
        // Pre-gap: form a bullish FVG.
        let pre = [
            b(0, 1.1000, 1.1010, 1.0990, 1.1005),
            b(60_000, 1.1015, 1.1030, 1.1010, 1.1025),
            b(120_000, 1.1030, 1.1050, 1.1020, 1.1045),
        ];
        for bar in &pre {
            eng.on_closed_bar(bar);
        }
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "pre-gap FVG should be present"
        );
        // Post-gap: jump 49 hours later (well beyond 3 × 5min). Single
        // post-gap bar — not enough to form a new FVG, AND the engine
        // must not synthesize one by combining pre-gap bars (the bug
        // the continuity guard prevents).
        let post = b(120_000 + 49 * 60 * 60_000, 1.0900, 1.0910, 1.0890, 1.0905);
        eng.on_closed_bar(&post);
        assert_eq!(
            eng.list_active("EURUSD", Timeframe::M5).len(),
            1,
            "pre-gap FVG should remain after gap; no phantom new FVG synthesized"
        );
    }

    #[test]
    fn pdh_sweep_uses_active_level_without_recomputing_pdh() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.hydrate(IctStructure::Pdh(LevelMarker {
            id: "pdh1".into(),
            symbol: "EURUSD".into(),
            tf: Timeframe::M1,
            price: 1.1000,
            label: "PDH".into(),
            confirmed_at_ts: Some(0),
            valid_from_ts: 0,
            valid_until_ts: i64::MAX,
            source_ts: Some(0),
        }));
        eng.on_closed_bar(&b(60_000, 1.0990, 1.1010, 1.0980, 1.0995));
        let rows = eng.list_active("EURUSD", Timeframe::M5);
        assert!(
            rows.iter().any(|s| matches!(s,
                IctStructure::LiquiditySweep(ls) if ls.pool_kind == LiquidityPoolKind::Pdh
            )),
            "expected PDH sweep, got {rows:?}"
        );
    }

    /// Reproduce the cold-start seed path (seed_closed_bar) and verify M4
    /// detectors actually emit. NY local midnight in July (EDT, UTC-4) =
    /// 04:00 UTC. Feed M1 bars spanning two midnights so PdhPdl rollover fires.
    #[test]
    fn seed_replay_emits_pdh_pdl_across_midnight() {
        use crate::detector::pdh_pdl::{DailyBoundary, PdhPdlDetector};
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "X",
            Timeframe::M1,
            Box::new(PdhPdlDetector::with_mode("X", DailyBoundary::NyLocal)),
        );
        let midnight_a: i64 = 1_720_238_400_000;
        let midnight_b: i64 = midnight_a + 86_400_000;
        let mut ts = midnight_a - 3_600_000;
        let end = ts + 50 * 3_600_000;
        let mut i = 0i64;
        while ts < end {
            let hi = 1.10 + (i % 60) as f64 * 0.001;
            let lo = 1.05 + (i % 60) as f64 * 0.0005;
            eng.seed_closed_bar(&Bar {
                symbol: "X".into(),
                tf: Timeframe::M1,
                ts,
                open: (hi + lo) / 2.0,
                high: hi,
                low: lo,
                close: (hi + lo) / 2.0,
                volume: 0.0,
            });
            ts += 60_000;
            i += 1;
        }
        let active = eng.list_active("X", Timeframe::M1);
        let has_pdh = active.iter().any(|s| matches!(s, IctStructure::Pdh(_)));
        let has_pdl = active.iter().any(|s| matches!(s, IctStructure::Pdl(_)));
        eprintln!(
            "SEEDTEST pdh={} pdl={} kinds={:?}",
            has_pdh,
            has_pdl,
            active.iter().map(|s| s.kind_tag()).collect::<Vec<_>>()
        );
        assert!(has_pdh, "PDH expected after seed across midnight");
        assert!(has_pdl, "PDL expected after seed across midnight");
        let _ = midnight_b;
    }

    /// BoS via the seed path: oscillation forming swings, then a close above
    /// the last swing high. BosDetector should emit.
    #[test]
    fn seed_replay_emits_bos_on_structure_break() {
        use crate::detector::bos::{BosConfig, BosDetector};
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(BosDetector::new(
                "EURUSD",
                Timeframe::M5,
                BosConfig::default(),
            )),
        );
        let ts = 1_000_000i64;
        let prices = [
            1.1000, 1.1010, 1.1020, 1.1030, 1.1020, 1.1010, 1.1000, 1.0990, 1.1000, 1.1010, 1.1020,
            1.1030, 1.1040, 1.1050, 1.1040, 1.1030, 1.1040, 1.1050, 1.1060, 1.1070, 1.1080, 1.1090,
            1.1100,
        ];
        for (i, &p) in prices.iter().enumerate() {
            let hi = p + 0.0005;
            let lo = p - 0.0005;
            eng.seed_closed_bar(&Bar {
                symbol: "EURUSD".into(),
                tf: Timeframe::M5,
                ts: ts + i as i64 * 300_000,
                open: p,
                high: hi,
                low: lo,
                close: p,
                volume: 0.0,
            });
        }
        let active = eng.list_active("EURUSD", Timeframe::M5);
        eprintln!(
            "SEEDTEST bos kinds={:?}",
            active.iter().map(|s| s.kind_tag()).collect::<Vec<_>>()
        );
        assert!(
            active.iter().any(|s| matches!(s, IctStructure::Bos(_))),
            "expected BoS after seed, kinds={:?}",
            active.iter().map(|s| s.kind_tag()).collect::<Vec<_>>()
        );
    }

    /// `has_symbol` flips true as soon as detectors are registered, but
    /// `has_bar_history` stays false until bars are actually fed. The
    /// watchlist-switch seed/hydrate paths must key off bar history (not
    /// mere registration) so a freshly bootstrapped symbol is still seeded
    /// from SQLite instead of being skipped - otherwise the chart stays
    /// blank until the TV Historical batch arrives.
    #[test]
    fn has_bar_history_distinguishes_registered_from_seeded() {
        let (tx, _rx) = broadcast::channel(64);
        let mut eng = IctEngine::new(tx);
        eng.register(
            "EURUSD",
            Timeframe::M5,
            Box::new(FvgDetector::new(
                "EURUSD",
                Timeframe::M5,
                FvgConfig::default(),
            )),
        );
        // Registered but no bars yet: has_symbol true, has_bar_history false.
        assert!(eng.has_symbol("EURUSD"));
        assert!(!eng.has_bar_history("EURUSD"));

        // Feed one closed bar -> now has bar history.
        eng.seed_closed_bar(&b(0, 1.1000, 1.1010, 1.0990, 1.1005));
        assert!(eng.has_bar_history("EURUSD"));

        // An unrelated symbol is neither registered nor seeded.
        assert!(!eng.has_symbol("GBPUSD"));
        assert!(!eng.has_bar_history("GBPUSD"));
    }
}
