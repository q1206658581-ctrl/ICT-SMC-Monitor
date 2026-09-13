//! Shared historical evidence reader for M8 evaluation and manual position drafts.
//! Read-only; delegates all price formulas to ContextPacker.
use super::{ContextPacker, LlmDecisionContext};
use crate::{
    candidate::{extract_ltf_events, PackedEvidence},
    detector::types::IctStructure,
    types::Bar,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};

pub fn pack_stored_evidence(
    conn: &Connection,
    mut evidence: PackedEvidence,
) -> Result<LlmDecisionContext> {
    let packer = ContextPacker::default();
    let as_of = evidence.as_of_ts;
    let symbol = evidence
        .trade_symbol
        .clone()
        .context("missing_trade_symbol")?;
    for sym in [&evidence.smt.sweeper_symbol, &symbol] {
        for (index, tf) in [
            evidence.smt.context_timeframe,
            evidence.smt.comparison_timeframe,
            evidence.candidate.validation_timeframe,
        ]
        .into_iter()
        .enumerate()
        {
            let mut query = conn.prepare("SELECT ts,open,high,low,close,volume FROM bars WHERE symbol=?1 AND tf=?2 AND ts<=?3 ORDER BY ts DESC LIMIT ?4")?;
            let bars = query.query_map(
                params![
                    sym,
                    tf.tag(),
                    as_of - tf.duration_ms(),
                    packer.bars_per_tf()[index] as i64
                ],
                |r| {
                    Ok(Bar {
                        symbol: sym.to_string(),
                        tf,
                        ts: r.get(0)?,
                        open: r.get(1)?,
                        high: r.get(2)?,
                        low: r.get(3)?,
                        close: r.get(4)?,
                        volume: r.get(5)?,
                    })
                },
            )?;
            for b in bars {
                evidence.market_bars.push(b?);
            }
            let mut query = conn.prepare("SELECT payload_json FROM ict_structures WHERE symbol=?1 AND (tf=?2 OR kind IN ('pdh','pdl','kill_zone','nwog','ndog','session_range','kill_zone_window')) ORDER BY ts_created,id")?;
            let structures = query.query_map(params![sym, tf.tag()], |r| r.get::<_, String>(0))?;
            for payload in structures {
                evidence.market_structures.push(
                    serde_json::from_str::<IctStructure>(&payload?)
                        .context("malformed_structure_payload")?,
                );
            }
        }
    }
    evidence
        .market_structures
        .sort_by(|a, b| a.id().cmp(b.id()));
    evidence.market_structures.dedup_by(|a, b| a.id() == b.id());
    let validation: Vec<_> = evidence
        .market_structures
        .iter()
        .filter(|s| s.tf() == evidence.candidate.validation_timeframe)
        .cloned()
        .collect();
    evidence.ltf_cisd_mss = extract_ltf_events(&validation);
    packer
        .pack_as_of(&evidence, as_of)
        .map_err(anyhow::Error::msg)
}
