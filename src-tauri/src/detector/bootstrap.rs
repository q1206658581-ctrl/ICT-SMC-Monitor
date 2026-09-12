//! Shared production detector registration.
//!
//! Live ingestion and the M8 replay harness must call this exact function so
//! detector coverage/configuration cannot drift between the two pipelines.

use crate::detector::bos::{BosConfig, BosDetector};
use crate::detector::cisd::{CisdConfig, CisdDetector};
use crate::detector::eqh_eql::{EqhEqlConfig, EqhEqlDetector};
use crate::detector::fvg::{FvgConfig, FvgDetector};
use crate::detector::liquidity_sweep::LiquiditySweepDetector;
use crate::detector::mss::{MssConfig, MssDetector};
use crate::detector::opening_gap::{OpeningGapConfig, OpeningGapDetector};
use crate::detector::order_block::{OrderBlockConfig, OrderBlockDetector};
use crate::detector::ote::{OteConfig, OteDetector};
use crate::detector::pdh_pdl::PdhPdlDetector;
use crate::detector::po3::{Po3Config, Po3Detector};
use crate::detector::premium_discount::{PremiumDiscountConfig, PremiumDiscountDetector};
use crate::detector::session::{SessionConfig, SessionDetector};
use crate::detector::volume_imbalance::{VolumeImbalanceConfig, VolumeImbalanceDetector};
use crate::detector::IctEngine;
use crate::types::Timeframe;

/// Register the detector set used by the production application.
///
/// Idempotency is per symbol: symbols already present in the engine are left
/// untouched. This matches live watchlist switching behavior.
pub fn register_production_detectors(engine: &mut IctEngine, symbols: &[String]) {
    let core_tfs = [
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::M30,
        Timeframe::H1,
        Timeframe::H4,
        Timeframe::D1,
        Timeframe::W1,
    ];

    for sym in symbols {
        if engine.has_symbol(sym) {
            continue;
        }
        engine.register(
            sym,
            Timeframe::M1,
            Box::new(PdhPdlDetector::new(sym.clone())),
        );
        engine.register(
            sym,
            Timeframe::M1,
            Box::new(OpeningGapDetector::new(
                sym.clone(),
                OpeningGapConfig::for_symbol(sym),
            )),
        );
        engine.register(
            sym,
            Timeframe::M1,
            Box::new(SessionDetector::new(sym.clone(), SessionConfig::default())),
        );
        for tf in core_tfs {
            if !matches!(tf, Timeframe::W1) {
                engine.register(
                    sym,
                    tf,
                    Box::new(Po3Detector::new(sym.clone(), tf, Po3Config::default())),
                );
            }
            engine.register(
                sym,
                tf,
                Box::new(FvgDetector::new(sym.clone(), tf, FvgConfig::default())),
            );
            engine.register(
                sym,
                tf,
                Box::new(OrderBlockDetector::new(
                    sym.clone(),
                    tf,
                    OrderBlockConfig::default(),
                )),
            );
            engine.register(
                sym,
                tf,
                Box::new(MssDetector::new(sym.clone(), tf, MssConfig::default())),
            );
            engine.register(
                sym,
                tf,
                Box::new(CisdDetector::new(sym.clone(), tf, CisdConfig::default())),
            );
            engine.register(
                sym,
                tf,
                Box::new(BosDetector::new(sym.clone(), tf, BosConfig::default())),
            );
            engine.register(
                sym,
                tf,
                Box::new(VolumeImbalanceDetector::new(
                    sym.clone(),
                    tf,
                    VolumeImbalanceConfig::default(),
                )),
            );
            engine.register(
                sym,
                tf,
                Box::new(OteDetector::new(sym.clone(), tf, OteConfig::default())),
            );
            engine.register(
                sym,
                tf,
                Box::new(PremiumDiscountDetector::new(
                    sym.clone(),
                    tf,
                    PremiumDiscountConfig::default(),
                )),
            );
            engine.register(
                sym,
                tf,
                Box::new(LiquiditySweepDetector::new(sym.clone(), tf)),
            );
            engine.register(
                sym,
                tf,
                Box::new(EqhEqlDetector::new(
                    sym.clone(),
                    tf,
                    EqhEqlConfig::default(),
                )),
            );
        }
    }
}
