use crate::types::Bar;

pub fn atr14(bars: &[Bar]) -> Option<f64> {
    if bars.len() < 15 {
        return None;
    }
    let start = bars.len().saturating_sub(14);
    let mut sum = 0.0;
    for i in start..bars.len() {
        let prev_close = bars
            .get(i.wrapping_sub(1))
            .map(|b| b.close)
            .unwrap_or(bars[i].close);
        let tr = (bars[i].high - bars[i].low)
            .max((bars[i].high - prev_close).abs())
            .max((bars[i].low - prev_close).abs());
        sum += tr;
    }
    Some(sum / 14.0)
}

pub fn pip_size(symbol: &str) -> f64 {
    let s = symbol.to_ascii_uppercase();
    if s.contains("BTC") {
        1.0
    } else if s.contains("ETH") {
        0.1
    } else if s.contains("JPY") {
        0.01
    } else if s.contains("XAU") || s.contains("XAG") {
        0.01
    } else {
        0.0001
    }
}

#[cfg(test)]
mod tests {
    use super::pip_size;

    #[test]
    fn pip_size_handles_crypto_symbols() {
        assert_eq!(pip_size("COINBASE:BTCUSD"), 1.0);
        assert_eq!(pip_size("BINANCE:ETHUSDT"), 0.1);
    }
}
