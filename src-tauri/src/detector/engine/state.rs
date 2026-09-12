use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use dashmap::{DashMap, DashSet};
use tokio::sync::broadcast;

use crate::types::{Bar, Timeframe};

use crate::detector::liquidity_reversal::LiquidityReversalConfig;
use crate::detector::swing::SwingSeries;
use crate::detector::types::{IctStructure, LiquiditySweep, StructureEvent};
use crate::detector::Detector;

/// Per-(symbol, tf) sliding history kept by the engine for detector input.
/// Sized to comfortably cover one previous civil day at the finest TF (1m)
/// **with margin** so PdhPdlDetector::flush — which scans the prev-day
/// window from this slice — never starts mid-day. 24h * 60 = 1440 minutes
/// for a NyLocal prev-day, 48h max for a Ny1700 prev-day-plus-buffer; 3000
/// gives ~50h of slack. Larger TFs use the same constant; their detectors
/// (FVG/OB/MSS/CISD) only consume the recent tail so the extra rows are
/// effectively idle. Memory cost: 7 TFs * 3000 Bars * ~64B ≈ 1.3 MB.
pub const DETECTOR_HISTORY: usize = 3000;

/// Soft per-bar latency budget (ms). When the engine spends longer than
/// this in a single `on_closed_bar` / `on_open_bar` call we emit a warn
/// log so regressions show up without impacting the live loop.
pub(super) const PER_BAR_BUDGET_MS: u128 = 5;

/// When the next closed bar arrives more than this many TF periods after
/// the previous closed bar (e.g. weekend gaps, market closure, dropped
/// feed), the per-(symbol, tf) detector state is treated as discontinuous.
/// History is truncated to the new bar, SwingSeries is reset, and each
/// detector's `reset()` is invoked. This prevents `detect_three_bar` from
/// straddling a weekend (the `i-2 / i / i-1` indices would otherwise pick
/// non-adjacent bars, producing impossible 49-hour FVGs — observed during
/// M3 acceptance, see `docs/M3_PROGRESS.md`).
pub(super) const MAX_BAR_GAP_MULTIPLIER: i64 = 3;

/// Detector configuration knob set at runtime by RightDrawer.
#[derive(Clone, Debug, Default)]
pub struct DetectorToggles {
    /// `name -> enabled`. Missing key = enabled by default.
    pub enabled: HashMap<String, bool>,
}

impl DetectorToggles {
    pub fn is_enabled(&self, name: &str) -> bool {
        *self.enabled.get(name).unwrap_or(&true)
    }
    pub fn set(&mut self, name: &str, enabled: bool) {
        self.enabled.insert(name.to_string(), enabled);
    }
}

pub(super) struct PerTf {
    pub(super) swings: SwingSeries,
    pub(super) detectors: Vec<Box<dyn Detector>>,
    pub(super) history: VecDeque<Bar>,
}

impl PerTf {
    pub(super) fn new() -> Self {
        Self {
            swings: SwingSeries::default(),
            detectors: Vec::new(),
            history: VecDeque::with_capacity(DETECTOR_HISTORY),
        }
    }
}

pub struct IctEngine {
    /// `(symbol, tf) -> per-TF state` (single SwingSeries shared by MSS+CISD).
    pub(super) streams: HashMap<(String, Timeframe), PerTf>,
    /// Active structures by stable id; persisted/queried for `list_structures`.
    pub structures: Arc<DashMap<String, IctStructure>>,
    pub event_tx: broadcast::Sender<StructureEvent>,
    pub toggles: DetectorToggles,
    pub(super) recent_sweeps: HashMap<(String, Timeframe), VecDeque<LiquiditySweep>>,
    pub(super) emitted_reversals: HashSet<String>,
    pub(super) swept_level_keys: HashSet<String>,
    pub(super) reversal_cfg: LiquidityReversalConfig,
    /// Symbols whose PO3 full-history replay has completed. After replay,
    /// PO3 is no longer skipped during seeding so that new bars arriving via
    /// TV Historical batches (which use `seed_closed_bar` / seeding=true)
    /// still increment `bars_waited` and invalidate stale PO3s.
    pub po3_replay_done: std::collections::HashSet<String>,
    /// When true, `apply_and_broadcast` applies events to engine state
    /// but skips the `event_tx` broadcast. Set during cold-start seed,
    /// TV Historical batch replay, and PO3 replay to avoid flooding the
    /// broadcast channel (capacity 64) with tens of thousands of
    /// historical events that the emit task cannot drain fast enough.
    /// The front-end re-fetches via `list_structures` after
    /// `seed_complete` / `po3_replay_done`.
    pub seeding: std::sync::atomic::AtomicBool,
    /// IDs created or updated while event broadcasting is suppressed.
    ///
    /// Historical replay cannot rely on the normal event consumer to mirror
    /// changes into SQLite.  Tracking only the affected IDs lets the caller
    /// persist the replay delta instead of serialising and UPSERTing the whole
    /// hydrated structure store (which can contain hundreds of thousands of
    /// historical rows).
    pub(super) seed_dirty_structure_ids: DashSet<String>,
    /// Secondary index: (symbol, tf) -> structure IDs. Lets
    /// detector_structures_snapshot avoid iterating all ~15k structures
    /// on every bar (was the cold-start bottleneck: 70-100s per (sym,tf)).
    pub structures_index: dashmap::DashMap<(String, crate::types::Timeframe), Vec<String>>,
}

impl IctEngine {
    pub fn new(event_tx: broadcast::Sender<StructureEvent>) -> Self {
        Self {
            streams: HashMap::new(),
            structures: Arc::new(DashMap::new()),
            event_tx,
            toggles: DetectorToggles::default(),
            recent_sweeps: HashMap::new(),
            emitted_reversals: HashSet::new(),
            swept_level_keys: HashSet::new(),
            reversal_cfg: LiquidityReversalConfig::default(),
            po3_replay_done: std::collections::HashSet::new(),
            structures_index: dashmap::DashMap::new(),
            seeding: std::sync::atomic::AtomicBool::new(false),
            seed_dirty_structure_ids: DashSet::new(),
        }
    }

    /// Drain structures changed while `seeding` was enabled.
    ///
    /// An ID can disappear before the drain when a replay invalidates a
    /// transient structure.  Such rows are intentionally omitted, matching
    /// the previous non-pruning snapshot behaviour.
    pub fn take_seed_dirty_structures(&self) -> Vec<IctStructure> {
        let ids: Vec<String> = self
            .seed_dirty_structure_ids
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        let mut structures = Vec::with_capacity(ids.len());
        for id in ids {
            self.seed_dirty_structure_ids.remove(&id);
            if let Some(structure) = self.structures.get(&id) {
                structures.push(structure.value().clone());
            }
        }
        structures
    }

    /// Ensure a per-(symbol, tf) bucket exists. Used at registration time and
    /// lazily on first bar.
    pub(super) fn ensure(&mut self, key: (String, Timeframe)) -> &mut PerTf {
        self.streams.entry(key).or_insert_with(PerTf::new)
    }

    /// Register a detector for (symbol, tf). M3-1 keeps this empty; M3-2+
    /// will call this from the bin to install FvgDetector, etc.
    #[allow(dead_code)]
    pub fn register(&mut self, symbol: &str, tf: Timeframe, det: Box<dyn Detector>) {
        let bucket = self.ensure((symbol.to_string(), tf));
        bucket.detectors.push(det);
    }

    /// Returns true if any detector bucket exists for `symbol` (used to
    /// make `bootstrap_detectors` idempotent across watchlist switches).
    pub fn has_symbol(&self, symbol: &str) -> bool {
        self.streams.keys().any(|(s, _)| s == symbol)
    }

    /// Returns true if any bucket for `symbol` already has bar history
    /// (i.e. the symbol has actually been fed bars, not merely had its
    /// detectors registered). `has_symbol` flips to true the moment
    /// `bootstrap_detectors` registers buckets, so it cannot distinguish
    /// "registered but unseeded" from "already seeded". This method can,
    /// and is used by the watchlist-switch seed/hydrate paths so a newly
    /// bootstrapped symbol still gets its SQLite history + structures
    /// replayed instead of being skipped (which left the chart blank
    /// until the TV Historical batch arrived).
    pub fn has_bar_history(&self, symbol: &str) -> bool {
        self.streams
            .iter()
            .any(|((s, _), b)| s == symbol && !b.history.is_empty())
    }

    /// Returns the timestamp of the last bar in the engine's bucket for
    /// (symbol, tf), or None if the bucket is empty.
    pub fn last_bar_ts(&self, symbol: &str, tf: Timeframe) -> Option<i64> {
        self.streams
            .get(&(symbol.to_string(), tf))
            .and_then(|b| b.history.back())
            .map(|b| b.ts)
    }

    /// Configure the swing fractal_n on a given (symbol, tf). Default 2.
    #[allow(dead_code)]
    pub fn set_swing_fractal(&mut self, symbol: &str, tf: Timeframe, n: usize) {
        let bucket = self.ensure((symbol.to_string(), tf));
        bucket.swings = SwingSeries::new(n);
    }

    #[cfg(test)]
    pub(crate) fn swing_fractal(&self, symbol: &str, tf: Timeframe) -> Option<usize> {
        self.streams
            .get(&(symbol.to_string(), tf))
            .map(|bucket| bucket.swings.fractal_n)
    }

    /// Clear bucket history + swings for a (symbol, tf) so the continuity
    /// guard does not skip bars during cold-start seeding.  Detector
    /// instances are reset so they start from a clean slate.
    pub fn clear_bucket_for_seeding(&mut self, symbol: &str, tf: Timeframe) {
        let key = (symbol.to_string(), tf);
        if let Some(bucket) = self.streams.get_mut(&key) {
            bucket.history.clear();
            let n = bucket.swings.fractal_n;
            bucket.swings = SwingSeries::new(n);
            for det in bucket.detectors.iter_mut() {
                det.reset();
            }
        }
    }
}
