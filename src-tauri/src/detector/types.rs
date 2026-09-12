//! Public types for the ICT detector engine.
//!
//! All structures the engine emits implement `Serialize` so the Tauri emit
//! task can ship them straight to the frontend without a manual mapping
//! layer. Field names map 1:1 onto the visual primitives in
//! `ui/src/components/chart/`.

use serde::{Deserialize, Serialize};

use crate::types::Timeframe;

/// Direction of an ICT structure (bullish/bearish never disappears even when
/// state machines flip into IFVG territory — keep the *original* leaning).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Bullish,
    Bearish,
}

impl Direction {
    pub fn opposite(self) -> Self {
        match self {
            Direction::Bullish => Direction::Bearish,
            Direction::Bearish => Direction::Bullish,
        }
    }
}

/// Pairwise correlation between two watchlist symbols (§5.2.15 SMT).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Correlation {
    Positive,
    Negative,
}

/// SMT detection lifecycle (M5_ADDENDUM_C2 §2.6).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SmtDetectionState {
    SmtKDetected,
    C2Confirmed,
    C3Entry,
    Invalidated,
}

/// Terminal reasons retained with an invalidated SMT audit row.
///
/// Keep these as structured values instead of a display string so the UI can
/// localize them and future audit tooling can group rows reliably.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SmtInvalidationReason {
    /// The sweeper symbol's MTF chain failed every C2 case after SMT K+1
    /// closed.
    SweeperC2Failed,
    /// DXY completed C2, but C3 made a new adverse extreme or closed back
    /// beyond the C1 reclaim boundary. The PDA reservation is released so a
    /// later, more adverse attempt can still use the same liquidity episode.
    SweeperC3Failed,
    /// Every non-sweeper symbol also swept the shared reference, so there was
    /// no cross-market divergence.
    AllCountersSwept,
    /// The persisted reference timestamp/price no longer matches canonical
    /// aligned child bars after aggregation repair or history replay.
    CanonicalReferenceChanged,
    /// Canonical aligned bars no longer reproduce the sweeper's liquidity
    /// touch/take.
    CanonicalSweepInvalid,
    /// The selected reference was already taken by an earlier interval and
    /// this was not a valid new-extreme retry after a released C2/C3 failure.
    ReferenceAlreadyTaken,
    /// One or more persisted per-symbol C1/SMT-K/C2/C3 candles no longer
    /// resolve exactly on canonical MTF history.
    CanonicalChainChanged,
    /// Another SMT reserving the same PDA completed a valid DXY C3 first and
    /// became that PDA's one confirmed setup.
    PdaConsumedByOtherSmt,
    /// A provisional SMT formed inside an open HTF candle, but later child
    /// candles changed its extreme or removed the divergence before close.
    HtfFormationCancelled,
}

impl SmtDetectionState {
    pub fn tag(self) -> &'static str {
        match self {
            SmtDetectionState::SmtKDetected => "smt_k_detected",
            SmtDetectionState::C2Confirmed => "c2_confirmed",
            SmtDetectionState::C3Entry => "c3_entry",
            SmtDetectionState::Invalidated => "invalidated",
        }
    }
}

/// Reference to a single candle (OHLC + ts) for C1/SMT K/C2/C3 chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandleRef {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// Per-symbol liquidity reference status (§1.3, §3.3).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiquidityRefStatus {
    Swept,
    NotSwept,
    EqualHighLow,
    Unknown,
}

/// One symbol's reference liquidity point + sweep status.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidityRef {
    pub symbol: String,
    pub ref_price: f64,
    pub ref_ts: i64,
    pub side: LiquiditySide,
    pub status: LiquidityRefStatus,
    pub tf: Timeframe, // HTF or confirmed local MTF reference timeframe
    /// Exact MTF candle that contributed this symbol's reference-side
    /// high/low inside the shared reference interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtf_ref_candle: Option<CandleRef>,
    /// Exact MTF candle that contributed this symbol's comparison-side
    /// high/low inside the shared HTF sweep interval. For DXY this is the
    /// canonical SMT K; counters retain their own true extreme for drawing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtf_sweep_candle: Option<CandleRef>,
}

/// Whether the reference liquidity was distant left-side or local near PDA (§3.4).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceScope {
    DistantLeftSide,
    LocalNearPda,
}

/// An additional valid DXY reference swept by the same completed HTF
/// candle. It is confluence evidence only and does not create another SMT.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SmtReferenceEvidence {
    pub ref_price: f64,
    pub ref_ts: i64,
    pub side: LiquiditySide,
    pub tf: Timeframe,
    pub scope: ReferenceScope,
}

/// Reference to the original DXY FVG PDA at the HTF context timeframe.
/// Includes zone coordinates so the frontend can draw the rectangle
/// without an additional structure lookup.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PdaRef {
    pub kind: String, // "ob" | "fvg"
    pub id: String,
    pub tf: Timeframe,
    pub direction: Direction, // bullish=BISI, bearish=SIBI
    pub price_low: f64,
    pub price_high: f64,
    pub ts_open: i64,
    pub ts_confirm: i64,
    /// Deterministic chart end for this SMT attempt. A valid C3 uses the end
    /// of its MTF candle; a failed attempt uses its resolution candle. The
    /// frontend only stamps the underlying FVG as consumed for valid C3 rows.
    /// None means the PDA is still reserved/active and extends to current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_ts: Option<i64>,
    /// When the FVG transitioned to Filled (None = never filled).
    /// Used during hydration to invalidate stale PDA refs whose FVG
    /// was filled before the SMT sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_filled: Option<i64>,
}

/// Per-symbol relative strength label (§4). "strong" or "weak" only —
/// never converted to trade recommendation (§13).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StrengthLabel {
    pub symbol: String,
    pub label: String, // "strong" | "weak"
}

/// Full FVG/IFVG state machine (§5.2.14).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FvgState {
    Active,
    Mitigated50,
    Filled,
    InvertedActive,
    InvertedMitigated,
}

impl FvgState {
    pub fn tag(self) -> &'static str {
        match self {
            FvgState::Active => "active",
            FvgState::Mitigated50 => "mitigated_50",
            FvgState::Filled => "filled",
            FvgState::InvertedActive => "inverted_active",
            FvgState::InvertedMitigated => "inverted_mitigated",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObState {
    Active,
    Tested,
    Mitigated,
}

impl ObState {
    pub fn tag(self) -> &'static str {
        match self {
            ObState::Active => "active",
            ObState::Tested => "tested",
            ObState::Mitigated => "mitigated",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ZoneState {
    Active,
    Tested,
    Mitigated,
    Filled,
    Invalidated,
}

impl ZoneState {
    pub fn tag(self) -> &'static str {
        match self {
            ZoneState::Active => "active",
            ZoneState::Tested => "tested",
            ZoneState::Mitigated => "mitigated",
            ZoneState::Filled => "filled",
            ZoneState::Invalidated => "invalidated",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GapState {
    Active,
    Mitigated50,
    Filled,
}

impl GapState {
    pub fn tag(self) -> &'static str {
        match self {
            GapState::Active => "active",
            GapState::Mitigated50 => "mitigated_50",
            GapState::Filled => "filled",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PdSide {
    Premium,
    Discount,
    Equilibrium,
}

impl PdSide {
    pub fn tag(self) -> &'static str {
        match self {
            PdSide::Premium => "premium",
            PdSide::Discount => "discount",
            PdSide::Equilibrium => "equilibrium",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpeningGapKind {
    Nwog,
    Ndog,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GapDirection {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Po3ContextKind {
    SessionAsia,
    GenericRange,
    HtfFvg,
    HtfOrderBlock,
    HtfBreaker,
    HtfOte,
    HtfPremiumDiscount,
    MixedHtfContext,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Po3State {
    AccumulationCandidate,
    ManipulationSwept,
    EarlyReversal,
    ReversalConfirmed,
    DistributionConfirmed,
}

impl Po3State {
    pub fn tag(self) -> &'static str {
        match self {
            Po3State::AccumulationCandidate => "accumulation_candidate",
            Po3State::ManipulationSwept => "manipulation_swept",
            Po3State::EarlyReversal => "early_reversal",
            Po3State::ReversalConfirmed => "reversal_confirmed",
            Po3State::DistributionConfirmed => "distribution_confirmed",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Po3Stage {
    Accumulation,
    Manipulation,
    Distribution,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Po3StageBox {
    pub stage: Po3Stage,
    pub ts_start: i64,
    pub ts_end: i64,
    pub price_low: f64,
    pub price_high: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Structure visible on the chart — wire-shape, not engine-internal.
///
/// `id` is a stable hex string derived from `blake3(symbol|tf|kind|...)` so
/// detectors can re-emit "Update" events without confusing the frontend.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IctStructure {
    Fvg(Fvg),
    OrderBlock(OrderBlock),
    Mss(Mss),
    Cisd(Cisd),
    Pdh(LevelMarker),
    Pdl(LevelMarker),
    KillZone(KillZoneSpan),
    LiquiditySweep(LiquiditySweep),
    EqualHighsLows(EqualHighsLows),
    LiquidityReversal(LiquidityReversal),
    BreakerBlock(BreakerBlock),
    Bos(Bos),
    VolumeImbalance(VolumeImbalance),
    Ote(OteZone),
    PremiumDiscount(PremiumDiscount),
    Nwog(GapZone),
    Ndog(GapZone),
    SessionRange(SessionRange),
    KillZoneWindow(KillZoneWindow),
    #[serde(rename = "power_of_3", alias = "power_of3")]
    PowerOf3(PowerOf3),
    SmtDivergence(SmtDivergence),
}

impl IctStructure {
    pub fn id(&self) -> &str {
        match self {
            IctStructure::Fvg(s) => &s.id,
            IctStructure::OrderBlock(s) => &s.id,
            IctStructure::Mss(s) => &s.id,
            IctStructure::Cisd(s) => &s.id,
            IctStructure::Pdh(s) => &s.id,
            IctStructure::Pdl(s) => &s.id,
            IctStructure::KillZone(s) => &s.id,
            IctStructure::LiquiditySweep(s) => &s.id,
            IctStructure::EqualHighsLows(s) => &s.id,
            IctStructure::LiquidityReversal(s) => &s.id,
            IctStructure::BreakerBlock(s) => &s.id,
            IctStructure::Bos(s) => &s.id,
            IctStructure::VolumeImbalance(s) => &s.id,
            IctStructure::Ote(s) => &s.id,
            IctStructure::PremiumDiscount(s) => &s.id,
            IctStructure::Nwog(s) => &s.id,
            IctStructure::Ndog(s) => &s.id,
            IctStructure::SessionRange(s) => &s.id,
            IctStructure::KillZoneWindow(s) => &s.id,
            IctStructure::PowerOf3(s) => &s.id,
            IctStructure::SmtDivergence(s) => &s.id,
        }
    }

    pub fn kind_tag(&self) -> &'static str {
        match self {
            IctStructure::Fvg(_) => "fvg",
            IctStructure::OrderBlock(_) => "order_block",
            IctStructure::Mss(_) => "mss",
            IctStructure::Cisd(_) => "cisd",
            IctStructure::Pdh(_) => "pdh",
            IctStructure::Pdl(_) => "pdl",
            IctStructure::KillZone(_) => "kill_zone",
            IctStructure::LiquiditySweep(_) => "liquidity_sweep",
            IctStructure::EqualHighsLows(_) => "equal_highs_lows",
            IctStructure::LiquidityReversal(_) => "liquidity_reversal",
            IctStructure::BreakerBlock(_) => "breaker_block",
            IctStructure::Bos(_) => "bos",
            IctStructure::VolumeImbalance(_) => "volume_imbalance",
            IctStructure::Ote(_) => "ote",
            IctStructure::PremiumDiscount(_) => "premium_discount",
            IctStructure::Nwog(_) => "nwog",
            IctStructure::Ndog(_) => "ndog",
            IctStructure::SessionRange(_) => "session_range",
            IctStructure::KillZoneWindow(_) => "kill_zone_window",
            IctStructure::PowerOf3(_) => "power_of_3",
            IctStructure::SmtDivergence(_) => "smt_divergence",
        }
    }

    pub fn state_tag(&self) -> &'static str {
        match self {
            IctStructure::Fvg(s) => s.state.tag(),
            IctStructure::OrderBlock(s) => s.state.tag(),
            IctStructure::BreakerBlock(s) => s.state.tag(),
            IctStructure::VolumeImbalance(s) => s.state.tag(),
            IctStructure::PremiumDiscount(s) => s.current_side.tag(),
            IctStructure::Nwog(s) | IctStructure::Ndog(s) => s.state.tag(),
            IctStructure::PowerOf3(s) => s.state.tag(),
            IctStructure::SessionRange(s) => {
                if s.finalized {
                    "finalized"
                } else {
                    "active"
                }
            }
            IctStructure::Mss(_)
            | IctStructure::Cisd(_)
            | IctStructure::Bos(_)
            | IctStructure::Ote(_)
            | IctStructure::KillZoneWindow(_)
            | IctStructure::Pdh(_)
            | IctStructure::Pdl(_)
            | IctStructure::KillZone(_)
            | IctStructure::LiquiditySweep(_)
            | IctStructure::LiquidityReversal(_) => "active",
            IctStructure::EqualHighsLows(s) => {
                if s.swept {
                    "swept"
                } else {
                    "active"
                }
            }
            IctStructure::SmtDivergence(s) => s
                .chains
                .iter()
                .map(|c| c.detection_state.tag())
                .next()
                .unwrap_or("invalidated"),
        }
    }

    pub fn symbol(&self) -> &str {
        match self {
            IctStructure::Fvg(s) => &s.symbol,
            IctStructure::OrderBlock(s) => &s.symbol,
            IctStructure::Mss(s) => &s.symbol,
            IctStructure::Cisd(s) => &s.symbol,
            IctStructure::Pdh(s) => &s.symbol,
            IctStructure::Pdl(s) => &s.symbol,
            IctStructure::KillZone(s) => &s.symbol,
            IctStructure::LiquiditySweep(s) => &s.symbol,
            IctStructure::EqualHighsLows(s) => &s.symbol,
            IctStructure::LiquidityReversal(s) => &s.symbol,
            IctStructure::BreakerBlock(s) => &s.symbol,
            IctStructure::Bos(s) => &s.symbol,
            IctStructure::VolumeImbalance(s) => &s.symbol,
            IctStructure::Ote(s) => &s.symbol,
            IctStructure::PremiumDiscount(s) => &s.symbol,
            IctStructure::Nwog(s) => &s.symbol,
            IctStructure::Ndog(s) => &s.symbol,
            IctStructure::SessionRange(s) => &s.symbol,
            IctStructure::KillZoneWindow(s) => &s.symbol,
            IctStructure::PowerOf3(s) => &s.symbol,
            IctStructure::SmtDivergence(s) => &s.sweeper_symbol,
        }
    }

    pub fn tf(&self) -> Timeframe {
        match self {
            IctStructure::Fvg(s) => s.tf,
            IctStructure::OrderBlock(s) => s.tf,
            IctStructure::Mss(s) => s.tf,
            IctStructure::Cisd(s) => s.tf,
            IctStructure::Pdh(s) => s.tf,
            IctStructure::Pdl(s) => s.tf,
            IctStructure::KillZone(s) => s.tf,
            IctStructure::LiquiditySweep(s) => s.tf,
            IctStructure::EqualHighsLows(s) => s.tf,
            IctStructure::LiquidityReversal(s) => s.tf,
            IctStructure::BreakerBlock(s) => s.tf,
            IctStructure::Bos(s) => s.tf,
            IctStructure::VolumeImbalance(s) => s.tf,
            IctStructure::Ote(s) => s.tf,
            IctStructure::PremiumDiscount(s) => s.tf,
            IctStructure::Nwog(s) => s.tf,
            IctStructure::Ndog(s) => s.tf,
            IctStructure::SessionRange(s) => s.tf,
            IctStructure::KillZoneWindow(s) => s.tf,
            IctStructure::PowerOf3(s) => s.tf,
            IctStructure::SmtDivergence(s) => s.comparison_timeframe,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fvg {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    pub ts_open: i64,
    pub ts_confirm: i64,
    pub price_low: f64,
    pub price_high: f64,
    pub state: FvgState,
    /// Timestamp of the bar that transitioned this FVG to `Filled`.
    /// `None` while the FVG is still `Active` or `Mitigated50`. Used by
    /// SMT PDA filtering to determine whether the FVG was still valid
    /// (pre-Filled) at the time of a sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_filled: Option<i64>,
    /// Set when this FVG is consumed by an SMT sweep: the ts of the first
    /// bar after the sweep that exits the FVG zone. Frontend uses this as
    /// the FVG box right edge and only renders FVGs with this field set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_exit_ts: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderBlock {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    pub ts_open: i64,
    pub ts_confirm: i64,
    pub price_low: f64,
    pub price_high: f64,
    pub state: ObState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mss {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    /// Bar where the close confirmed the break.
    pub break_ts: i64,
    pub break_price: f64,
    /// The swing that got broken.
    pub swing_ts: i64,
    pub swing_price: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cisd {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    pub leg_origin_ts: i64,
    pub leg_origin_price: f64,
    pub break_ts: i64,
    pub break_price: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bos {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    pub break_ts: i64,
    pub break_price: f64,
    pub swing_ts: i64,
    pub swing_price: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BreakerBlock {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    pub source_ob_id: String,
    pub ts_open: i64,
    pub ts_confirm: i64,
    pub price_low: f64,
    pub price_high: f64,
    pub state: ZoneState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeImbalance {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    pub ts_open: i64,
    pub ts_confirm: i64,
    pub price_low: f64,
    pub price_high: f64,
    pub state: GapState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OteZone {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    pub leg_start_ts: i64,
    pub leg_end_ts: i64,
    pub leg_low: f64,
    pub leg_high: f64,
    pub price_low: f64,
    pub price_high: f64,
    pub fib_low: f64,
    pub fib_high: f64,
    pub confluent_structure_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PremiumDiscount {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub range_start_ts: i64,
    pub range_end_ts: i64,
    pub high: f64,
    pub low: f64,
    pub equilibrium: f64,
    pub current_side: PdSide,
    pub current_price: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GapZone {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    #[serde(rename = "gap_kind")]
    pub kind: OpeningGapKind,
    pub direction: GapDirection,
    pub ts_start: i64,
    pub ts_end: i64,
    pub prev_close_ts: i64,
    pub new_open_ts: i64,
    pub prev_close: f64,
    pub new_open: f64,
    pub price_low: f64,
    pub price_high: f64,
    pub state: GapState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LevelMarker {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub price: f64,
    pub label: String,
    /// Market close at which the source day's completed level became known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_at_ts: Option<i64>,
    pub valid_from_ts: i64,
    pub valid_until_ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ts: Option<i64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Asia,
    LondonOpen,
    NewYorkOpen,
    LondonClose,
}

impl SessionKind {
    pub fn label(self) -> &'static str {
        match self {
            SessionKind::Asia => "Asia",
            SessionKind::LondonOpen => "LO",
            SessionKind::NewYorkOpen => "NY",
            SessionKind::LondonClose => "LC",
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            SessionKind::Asia => "asia",
            SessionKind::LondonOpen => "london_open",
            SessionKind::NewYorkOpen => "new_york_open",
            SessionKind::LondonClose => "london_close",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KillZoneWindow {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub session: SessionKind,
    pub label: String,
    pub ts_start: i64,
    pub ts_end: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRange {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub session: SessionKind,
    pub label: String,
    pub ts_start: i64,
    pub ts_end: i64,
    pub high: f64,
    pub low: f64,
    pub high_ts: i64,
    pub low_ts: i64,
    pub finalized: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PowerOf3 {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    pub state: Po3State,
    pub context_kind: Po3ContextKind,
    pub context_structure_ids: Vec<String>,
    pub context_timeframes: Vec<Timeframe>,
    pub accumulation_start_ts: i64,
    pub accumulation_end_ts: i64,
    pub accumulation_high: f64,
    pub accumulation_low: f64,
    pub sweep_ts: i64,
    pub sweep_price: f64,
    pub confirm_ts: i64,
    pub confirm_id: String,
    pub confirm_kind: ReversalConfirmKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_ts: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_price: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_tf: Option<Timeframe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_kind: Option<ReversalConfirmKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bos_id: Option<String>,
    pub quality_score: u8,
    pub stage_boxes: Vec<Po3StageBox>,
}

/// One symbol's C1/SMT K/C2/C3 chain (M5_ADDENDUM_C2 §2.2, §3).
/// DXY owns the setup lifecycle, but every symbol evaluates its own local
/// C1/SMT-K/C2/C3 chain from the MTF candle that made that symbol's HTF
/// comparison extreme. Their timestamps intentionally may differ.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolChain {
    pub symbol: String,
    pub c1_candle: CandleRef,
    pub smt_k_candle: CandleRef,
    pub c2_candle: Option<CandleRef>,
    pub c2_case: Option<u8>, // 1 | 2 | 3
    pub c3_candle: Option<CandleRef>,
    pub detection_state: SmtDetectionState,
}

/// SMT divergence with multi-chain C1/SMT K/C2/C3 (M5_ADDENDUM_C2 §3).
///
/// Each involved symbol has its own chain in `chains`. `sweeper_symbol` =
/// DXY (trigger); `trade_symbols` = EU/GU (entry target; all non-swept counters).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SmtDivergence {
    // 标识
    pub id: String,
    pub watchlist_id: String,
    pub rule_version: String,

    // 比较组与关系 (§2.3, §3)
    pub symbol_set: Vec<String>,
    pub relationship: Correlation, // Positive | Negative (Negative = Inverse)
    pub context_timeframe: Timeframe, // PDA 所在 HTF (上一级)
    pub comparison_timeframe: Timeframe, // SMT/C2 所在 MTF
    pub observation_window: (i64, i64), // 跨品种比较窗口 start/end
    /// False while the parent HTF candle is still forming. Old persisted
    /// payloads predate live provisional SMTs and therefore default to true.
    #[serde(default = "default_true")]
    pub htf_confirmed: bool,

    // 参照流动性 (§3.3, §3.4)
    pub reference_scope: ReferenceScope,
    pub liquidity_refs: Vec<LiquidityRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confluence_refs: Vec<SmtReferenceEvidence>,

    // 方向与强弱 (§7)
    pub candidate_direction: Direction, // Bullish | Bearish
    pub sweeper_symbol: String,         // DXY（4h PDA 扫者/触发/锚；KB 案例都在 DXY）
    #[serde(default)]
    pub trade_symbols: Vec<String>, // 入场目标 EU/GU（未扫的对手品种，可能多个）
    pub strength: Vec<StrengthLabel>,

    // C1 / SMT K / C2 / C3 (§5；每品种各自一条链)
    pub chains: Vec<SymbolChain>, // 每品种一条：DXY + EU/GU 各自 C1/SMT K/C2/C3

    // Terminal audit metadata. Old payloads deserialize to an empty vector;
    // the UI labels those honestly instead of guessing a reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalidation_reasons: Vec<SmtInvalidationReason>,
    /// Market timestamp of the candle/HTF-close that made this SMT terminal.
    /// Kept in the snapshot so live processing and historical replay produce
    /// identical Candidate decision IDs and audit timestamps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidation_ts: Option<i64>,

    // DXY HTF FVG PDA：当前正式 SMT 的成立条件之一。
    pub htf_pda_ref: Option<PdaRef>,

    // MTF 参照极值 K (§5.8 MTF 层白线起点): DXY 在 HTF 参照 K 时间区间
    // [ref_ts, ref_ts+htf) 内做出极值的那根 MTF K (buy_side 取 high 最大,
    // sell_side 取 low 最小)。由 sweeper 决定, 三品种共享此时间; EU/GU 用
    // 同一时间取各自 high/low 画 MTF 层白线。None = 未计算(回退 ref_ts)。
    pub mtf_ref_candle: Option<CandleRef>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KillZoneKind {
    Asia,
    LondonOpen,
    NewYorkOpen,
    LondonClose,
    SilverBulletAsia,
    SilverBulletLondon,
    SilverBulletNewYork,
}

impl KillZoneKind {
    pub fn label(self) -> &'static str {
        match self {
            KillZoneKind::Asia => "AKZ",
            KillZoneKind::LondonOpen => "LOKZ",
            KillZoneKind::NewYorkOpen => "NYOKZ",
            KillZoneKind::LondonClose => "LCKZ",
            KillZoneKind::SilverBulletAsia => "SB-AM",
            KillZoneKind::SilverBulletLondon => "SB-LO",
            KillZoneKind::SilverBulletNewYork => "SB-PM",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KillZoneSpan {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    #[serde(rename = "kz_kind")]
    pub kind: KillZoneKind,
    pub label: String,
    pub ts_start: i64,
    pub ts_end: i64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LiquidityPoolKind {
    SwingHigh,
    SwingLow,
    EqualHighs,
    EqualLows,
    Pdh,
    Pdl,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LiquiditySide {
    BuySide,
    SellSide,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquiditySweep {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub side: LiquiditySide,
    pub pool_kind: LiquidityPoolKind,
    pub sweep_ts: i64,
    pub sweep_price: f64,
    pub level_ts: i64,
    pub level_price: f64,
    pub close_price: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EqualHighsLows {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub side: LiquiditySide,
    pub ts_start: i64,
    pub ts_end: i64,
    pub price: f64,
    pub tolerance_price: f64,
    /// Close boundary at which both swings were actually confirmed.
    /// Legacy rows lack this fact and must not be historical target inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_at_ts: Option<i64>,
    pub swept: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ReversalConfirmKind {
    Cisd,
    Mss,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidityReversal {
    pub id: String,
    pub symbol: String,
    pub tf: Timeframe,
    pub direction: Direction,
    pub sweep_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_pool_kind: Option<LiquidityPoolKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_side: Option<LiquiditySide>,
    pub confirm_id: String,
    pub confirm_kind: ReversalConfirmKind,
    pub sweep_ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_level_ts: Option<i64>,
    pub confirm_ts: i64,
    pub level_price: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_price: Option<f64>,
    pub score: u8,
}

/// Wire-event the engine broadcasts to subscribers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StructureEvent {
    New(IctStructure),
    Update(IctStructure),
    Invalidated { id: String, kind: String },
}

impl StructureEvent {
    pub fn topic(&self) -> &'static str {
        match self {
            StructureEvent::New(_) => "ict:structure:new",
            StructureEvent::Update(_) => "ict:structure:update",
            StructureEvent::Invalidated { .. } => "ict:structure:invalidated",
        }
    }
}

/// Stable id helper. Same inputs → same hex string across processes.
pub fn structure_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            hasher.update(b"|");
        }
        hasher.update(p.as_bytes());
    }
    let h = hasher.finalize();
    hex16(h.as_bytes())
}

fn hex16(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(32);
    for &b in &bytes[..16] {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}
