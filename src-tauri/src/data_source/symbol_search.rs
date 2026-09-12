//! TradingView symbol catalogue. No cookies/API keys are sent to this public endpoint.
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchSymbol {
    pub symbol: String,
    pub description: String,
    pub exchange: String,
    pub kind: String,
}

pub fn validate_symbol(symbol: &str) -> Result<(), String> {
    let Some((source, ticker)) = symbol.split_once(':') else {
        return Err("请选择带交易所或数据源前缀的品种，例如 OANDA:EURUSD".into());
    };
    if source.is_empty() || ticker.is_empty() || symbol.len() > 128
        || !source.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || ticker.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '"' | '\\' | '<' | '>')) {
        return Err("品种代码格式不正确，请从搜索结果中选择".into());
    }
    Ok(())
}

pub async fn search(query: &str, proxy: Option<&str>) -> Result<Vec<SearchSymbol>, String> {
    let query = query.trim();
    if query.is_empty() { return Ok(Vec::new()); }
    if query.chars().count() > 80 { return Err("搜索内容最多 80 个字符".into()); }
    let (exchange, text) = query.split_once(':').unwrap_or(("", query));
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(15));
    if let Some(proxy) = proxy.filter(|s| !s.is_empty()) {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| "行情代理配置无效")?);
    }
    let response = builder.build().map_err(|_| "无法初始化品种搜索")?
        .get("https://symbol-search.tradingview.com/symbol_search/v3/")
        .header("Origin", "https://www.tradingview.com")
        .header("User-Agent", "Mozilla/5.0")
        .query(&[("text", text), ("exchange", exchange), ("lang", "en"), ("domain", "production")])
        .send().await.map_err(|e| if e.is_timeout() { "品种搜索超时，请重试" } else { "无法连接 TradingView 搜索，请检查行情代理或网络" })?;
    if !response.status().is_success() {
        return Err(format!("TradingView 搜索返回 HTTP {}，请稍后重试", response.status().as_u16()));
    }
    let payload: serde_json::Value = response.json().await.map_err(|_| "TradingView 搜索返回格式无效")?;
    parse_results(&payload)
}

fn parse_results(payload: &serde_json::Value) -> Result<Vec<SearchSymbol>, String> {
    let rows = payload.get("symbols").and_then(|v| v.as_array()).ok_or("搜索结果缺少品种列表")?;
    let mut results = Vec::new();
    for row in rows.iter().take(50) {
        let ticker = row.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
        let exchange = row.get("prefix").or_else(|| row.get("exchange")).and_then(|v| v.as_str()).unwrap_or("");
        let symbol = format!("{exchange}:{ticker}");
        if validate_symbol(&symbol).is_err() || results.iter().any(|r: &SearchSymbol| r.symbol == symbol) { continue; }
        results.push(SearchSymbol {
            symbol,
            exchange: exchange.into(),
            description: row.get("description").and_then(|v| v.as_str()).unwrap_or(ticker).into(),
            kind: row.get("type").and_then(|v| v.as_str()).unwrap_or("").into(),
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_distinct_sources_and_rejects_malformed_symbols() {
        let rows = parse_results(&serde_json::json!({"symbols":[
            {"symbol":"XAUUSD","exchange":"OANDA","description":"Gold","type":"commodity"},
            {"symbol":"XAUUSD","exchange":"FXCM"},
            {"symbol":"XAUUSD","exchange":"OANDA"},
            {"symbol":"BAD CODE","exchange":"TEST"}
        ]})).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].symbol, "OANDA:XAUUSD");
        assert_eq!(rows[1].symbol, "FXCM:XAUUSD");
        assert!(validate_symbol("EURUSD").is_err());
        assert!(validate_symbol("CME_MINI:ES1!").is_ok());
    }
}
