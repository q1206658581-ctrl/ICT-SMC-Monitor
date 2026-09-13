//! Manual annotations. Never consumed by detectors, alerts or provider dispatch.
use crate::llm::{DeterministicDecisionGuardrails, LlmDecisionDirection};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PositionSide {
    Long,
    Short,
}
impl PositionSide {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Short => "short",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PositionPrices {
    pub side: PositionSide,
    pub entry_price: f64,
    pub stop_price: f64,
    pub target_price: Option<f64>,
}
impl PositionPrices {
    pub fn validate(&self) -> Result<()> {
        if !self.entry_price.is_finite()
            || !self.stop_price.is_finite()
            || self.target_price.is_some_and(|p| !p.is_finite())
        {
            bail!("价格必须是有效数字");
        }
        let direction = if self.side == PositionSide::Long {
            1.0
        } else {
            -1.0
        };
        if (self.entry_price - self.stop_price) * direction <= 0.0 {
            bail!("多头止损必须低于入场价；空头止损必须高于入场价");
        }
        if self
            .target_price
            .is_some_and(|p| (p - self.entry_price) * direction <= 0.0)
        {
            bail!("多头止盈必须高于入场价；空头止盈必须低于入场价");
        }
        if !(self.entry_price - self.stop_price).is_finite()
            || self
                .target_price
                .is_some_and(|p| !(p - self.entry_price).is_finite())
        {
            bail!("价格间距超出范围");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserPosition {
    pub id: String,
    pub symbol: String,
    #[serde(flatten)]
    pub prices: PositionPrices,
    pub source_alert_id: Option<String>,
    pub created_at_ts: i64,
    pub updated_at_ts: i64,
    pub drawn_tf: Option<String>,
    pub anchor_ts: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PositionDraft {
    pub symbol: String,
    #[serde(flatten)]
    pub prices: PositionPrices,
    pub source_alert_id: String,
    pub drawn_tf: Option<String>,
    pub anchor_ts: Option<i64>,
}

pub fn prices_from_guardrails(g: &DeterministicDecisionGuardrails) -> Result<PositionPrices> {
    let zone = g
        .entry_zone
        .as_ref()
        .context("该告警没有可用的确定性入场区，请手动画仓位")?;
    if !zone.low.is_finite() || !zone.high.is_finite() || zone.low > zone.high {
        bail!("入场区无效");
    }
    let prices = PositionPrices {
        side: match g.direction {
            LlmDecisionDirection::Bullish => PositionSide::Long,
            LlmDecisionDirection::Bearish => PositionSide::Short,
            LlmDecisionDirection::Neutral => bail!("该告警没有明确的确定性交易方向"),
        },
        entry_price: (zone.low + zone.high) / 2.0,
        stop_price: g
            .invalidation_price
            .context("该告警没有可用的确定性失效位，请手动画仓位")?,
        target_price: g.targets.first().map(|t| t.price),
    };
    prices.validate()?;
    Ok(prices)
}
