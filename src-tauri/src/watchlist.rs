//! Watchlist domain types, built-in defaults, default correlation table,
//! and validation (M5 kickoff §3 / §4.4 / M5_PLAN §内置 watchlist / §默认相关性表).

use serde::{Deserialize, Serialize};

use crate::detector::Correlation;

/// A persisted watchlist entry stored under `[[watchlists]]` in config.toml.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Watchlist {
    pub id: String,
    pub name: String,
    pub symbols: Vec<String>,
    #[serde(default)]
    pub correlations: Vec<PairCorrelation>,
}

/// One pairwise correlation row (`a` < `b` lexicographically).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairCorrelation {
    pub a: String,
    pub b: String,
    pub direction: Correlation,
}

impl PairCorrelation {
    /// Sort the two symbols so `a` is always lexicographically smaller.
    pub fn sorted(sym_a: &str, sym_b: &str, direction: Correlation) -> Self {
        let (a, b) = if sym_a <= sym_b {
            (sym_a.to_string(), sym_b.to_string())
        } else {
            (sym_b.to_string(), sym_a.to_string())
        };
        Self { a, b, direction }
    }
}

/// Input for create/update watchlist commands.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchlistInput {
    pub name: String,
    pub symbols: Vec<String>,
    pub correlations: Vec<PairCorrelation>,
}

/// Result of `default_correlation` IPC — `known = false` when the pair is not
/// in the built-in table (UI shows a "please confirm direction" warning).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DefaultCorrelation {
    pub direction: Correlation,
    pub known: bool,
}

impl Watchlist {
    /// Validate symbol count, pair completeness, lexicographic ordering, and
    /// symbol name format. Returns a human-readable error on failure.
    pub fn validate(&self) -> Result<(), String> {
        if self.symbols.is_empty() || self.symbols.len() > 3 {
            return Err(format!(
                "watchlist '{}' must have 1-3 symbols, has {}",
                self.id,
                self.symbols.len()
            ));
        }
        // No duplicate symbols.
        let mut seen = std::collections::HashSet::new();
        for s in &self.symbols {
            if !is_valid_symbol(s) {
                return Err(format!("watchlist '{}': invalid symbol '{s}'", self.id));
            }
            if !seen.insert(s.clone()) {
                return Err(format!("watchlist '{}': duplicate symbol '{s}'", self.id));
            }
        }
        // Correlation completeness: exactly N*(N-1)/2 pairs.
        let n = self.symbols.len();
        let expected = n * (n - 1) / 2;
        if self.correlations.len() != expected {
            return Err(format!(
                "watchlist '{}': expected {expected} correlation pairs, has {}",
                self.id,
                self.correlations.len()
            ));
        }
        let mut pair_keys = std::collections::HashSet::new();
        for c in &self.correlations {
            if c.a >= c.b {
                return Err(format!(
                    "watchlist '{}': pair ('{}','{}') not in lexicographic order (a must be < b)",
                    self.id, c.a, c.b
                ));
            }
            if !self.symbols.contains(&c.a) || !self.symbols.contains(&c.b) {
                return Err(format!(
                    "watchlist '{}': pair ('{}','{}') references a symbol not in the watchlist",
                    self.id, c.a, c.b
                ));
            }
            if !pair_keys.insert((c.a.clone(), c.b.clone())) {
                return Err(format!(
                    "watchlist '{}': duplicate correlation pair ('{}','{}')",
                    self.id, c.a, c.b
                ));
            }
        }
        let expected_pairs: std::collections::HashSet<(String, String)> = self
            .symbols
            .iter()
            .enumerate()
            .flat_map(|(i, a)| {
                self.symbols.iter().skip(i + 1).map(move |b| {
                    if a <= b {
                        (a.clone(), b.clone())
                    } else {
                        (b.clone(), a.clone())
                    }
                })
            })
            .collect();
        if pair_keys != expected_pairs {
            return Err(format!(
                "watchlist '{}': correlation rows do not cover every symbol pair exactly once",
                self.id
            ));
        }
        Ok(())
    }
}

/// A symbol is valid if it has an `EXCHANGE:TICKER` shape (non-empty on both
/// sides of the colon).
pub fn is_valid_symbol(s: &str) -> bool {
    match s.split_once(':') {
        Some((ex, tk)) => !ex.is_empty() && !tk.is_empty(),
        None => false,
    }
}

/// Strip the exchange prefix, returning just the ticker (`OANDA:EURUSD` → `EURUSD`).
fn ticker(symbol: &str) -> &str {
    symbol.split_once(':').map(|(_, t)| t).unwrap_or(symbol)
}

/// Built-in default correlation table (M5_PLAN §默认相关性表).
/// Tickers are compared after stripping the exchange prefix.
/// Stored as `(ticker_a, ticker_b, direction)` with `a < b` lexicographically.
const DEFAULT_CORRELATIONS: &[(&str, &str, Correlation)] = &[
    // EURUSD ↔ GBPUSD / AUDUSD / NZDUSD  → positive
    ("AUDUSD", "EURUSD", Correlation::Positive),
    ("AUDUSD", "GBPUSD", Correlation::Positive),
    ("EURUSD", "GBPUSD", Correlation::Positive),
    ("EURUSD", "NZDUSD", Correlation::Positive),
    ("GBPUSD", "NZDUSD", Correlation::Positive),
    ("AUDUSD", "NZDUSD", Correlation::Positive),
    // EURUSD ↔ USDCHF / USDCAD / USDJPY / DXY → negative
    ("DXY", "EURUSD", Correlation::Negative),
    ("EURUSD", "USDCAD", Correlation::Negative),
    ("EURUSD", "USDCHF", Correlation::Negative),
    ("EURUSD", "USDJPY", Correlation::Negative),
    // GBPUSD ↔ USDCHF / USDCAD / USDJPY / DXY → negative
    ("DXY", "GBPUSD", Correlation::Negative),
    ("GBPUSD", "USDCAD", Correlation::Negative),
    ("GBPUSD", "USDCHF", Correlation::Negative),
    ("GBPUSD", "USDJPY", Correlation::Negative),
    // AUDUSD ↔ USDCAD → negative
    ("AUDUSD", "DXY", Correlation::Negative),
    ("AUDUSD", "USDCAD", Correlation::Negative),
    // NZDUSD ↔ DXY → negative
    ("DXY", "NZDUSD", Correlation::Negative),
    // USDCHF ↔ USDCAD / USDJPY / DXY → positive
    ("DXY", "USDCHF", Correlation::Positive),
    ("USDCHF", "USDCAD", Correlation::Positive),
    ("USDJPY", "USDCHF", Correlation::Positive),
    // USDCAD ↔ USDJPY / DXY → positive
    ("DXY", "USDCAD", Correlation::Positive),
    ("DXY", "USDJPY", Correlation::Positive),
    // XAUUSD ↔ XAGUSD → positive ; XAUUSD ↔ DXY → negative
    ("XAUUSD", "XAGUSD", Correlation::Positive),
    ("DXY", "XAUUSD", Correlation::Negative),
    // BTCUSD ↔ DXY -> negative (BTC priced in USD)
    ("BTCUSD", "DXY", Correlation::Negative),
];

/// Look up the default correlation for a pair. Returns `Positive` + `known=false`
/// when the pair is not in the table (caller/UI should prompt the user).
pub fn default_correlation(sym_a: &str, sym_b: &str) -> DefaultCorrelation {
    let ta = ticker(sym_a);
    let tb = ticker(sym_b);
    let (a, b) = if ta <= tb { (ta, tb) } else { (tb, ta) };
    for (ea, eb, dir) in DEFAULT_CORRELATIONS {
        if *ea == a && *eb == b {
            return DefaultCorrelation {
                direction: *dir,
                known: true,
            };
        }
    }
    DefaultCorrelation {
        direction: Correlation::Positive,
        known: false,
    }
}

/// Build the correlation rows for a set of symbols using the default table.
pub fn default_correlations_for(symbols: &[String]) -> Vec<PairCorrelation> {
    let mut out = Vec::new();
    for i in 0..symbols.len() {
        for j in (i + 1)..symbols.len() {
            let d = default_correlation(&symbols[i], &symbols[j]);
            out.push(PairCorrelation::sorted(
                &symbols[i],
                &symbols[j],
                d.direction,
            ));
        }
    }
    out
}

/// Built-in watchlists monitored concurrently from M6c onward.
pub fn default_watchlists() -> Vec<Watchlist> {
    let defs: &[(&str, &str, &[&str])] = &[
        (
            "eu-gu-dxy",
            "EU / GU / DXY",
            &["OANDA:EURUSD", "OANDA:GBPUSD", "TVC:DXY"],
        ),
        (
            "aud-nzd-dxy",
            "AUD / NZD / DXY",
            &["OANDA:AUDUSD", "OANDA:NZDUSD", "TVC:DXY"],
        ),
        (
            "chf-cad-dxy",
            "CHF / CAD / DXY",
            &["OANDA:USDCHF", "OANDA:USDCAD", "TVC:DXY"],
        ),
    ];
    defs.iter()
        .map(|(id, name, syms)| {
            let symbols: Vec<String> = syms.iter().map(|s| s.to_string()).collect();
            Watchlist {
                id: id.to_string(),
                name: name.to_string(),
                symbols: symbols.clone(),
                correlations: default_correlations_for(&symbols),
            }
        })
        .collect()
}

/// Add built-in watchlists that are missing (by id) without duplicating
/// existing ones. Used by `restore_default_watchlists`.
pub fn merge_defaults(existing: &[Watchlist]) -> Vec<Watchlist> {
    let have: std::collections::HashSet<&str> = existing.iter().map(|w| w.id.as_str()).collect();
    let mut out = existing.to_vec();
    for w in default_watchlists() {
        if !have.contains(w.id.as_str()) {
            out.push(w);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m6c_defaults_have_three_valid_groups_and_explicit_dxy_pairs() {
        let groups = default_watchlists();
        assert_eq!(groups.len(), 3);
        assert!(groups.iter().all(|group| group.validate().is_ok()));

        for symbol in ["OANDA:AUDUSD", "OANDA:NZDUSD"] {
            let corr = default_correlation(symbol, "TVC:DXY");
            assert!(corr.known, "{symbol} ↔ DXY must not use fallback");
            assert_eq!(corr.direction, Correlation::Negative);
        }
        for symbol in ["OANDA:USDCHF", "OANDA:USDCAD"] {
            let corr = default_correlation(symbol, "TVC:DXY");
            assert!(corr.known);
            assert_eq!(corr.direction, Correlation::Positive);
        }
    }
}
