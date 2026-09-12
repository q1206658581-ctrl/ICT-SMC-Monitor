pub(super) fn detector_kind_tags(name: &str) -> &'static [&'static str] {
    match name {
        "fvg" => &["fvg"],
        "order_block" => &["order_block"],
        "mss" => &["mss"],
        "cisd" => &["cisd"],
        "pdh_pdl" => &["pdh", "pdl"],
        "liquidity" => &["liquidity_sweep", "equal_highs_lows"],
        "liquidity_reversal" => &["liquidity_reversal"],
        "bos" => &["bos"],
        "volume_imbalance" => &["volume_imbalance"],
        "ote" => &["ote"],
        "breaker_block" => &["breaker_block"],
        "premium_discount" => &["premium_discount"],
        "opening_gap" => &["nwog", "ndog"],
        "session" => &["session_range", "kill_zone_window"],
        "po3" => &["power_of_3"],
        _ => &[],
    }
}
