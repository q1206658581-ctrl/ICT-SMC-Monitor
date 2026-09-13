use super::SqliteStore;
use crate::{
    candidate::{CandidateSetup, PackedEvidence},
    detector::types::SmtDivergence,
    llm::{
        pack_stored_evidence, DeterministicDecisionGuardrails, M7D_CONTEXT_VERSION,
        M7D_STRATEGY_VERSION,
    },
    positions::{
        prices_from_guardrails, PositionDraft, PositionPrices, PositionSide, UserPosition,
    },
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserPosition> {
    let side: String = r.get(2)?;
    let side = match side.as_str() {
        "long" => PositionSide::Long,
        "short" => PositionSide::Short,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(UserPosition {
        id: r.get(0)?,
        symbol: r.get(1)?,
        prices: PositionPrices {
            side,
            entry_price: r.get(3)?,
            stop_price: r.get(4)?,
            target_price: r.get(5)?,
        },
        source_alert_id: r.get(6)?,
        created_at_ts: r.get(7)?,
        updated_at_ts: r.get(8)?,
        drawn_tf: r.get(9)?,
        anchor_ts: r.get(10)?,
    })
}
const COLUMNS: &str = "id,symbol,side,entry_price,stop_price,target_price,source_alert_id,created_at_ts,updated_at_ts,drawn_tf,anchor_ts";

// Freeze the C2 close time, not the later insertion/validation time.
fn source_anchor(c: &rusqlite::Connection, alert_id: &str) -> Result<Option<i64>> {
    let has_alerts: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='alerts_fired' AND type='table')",
        [],
        |r| r.get(0),
    )?;
    if !has_alerts {
        return Ok(None);
    }
    let row: Option<(i64, String)> = c
        .query_row(
            "SELECT c2_candle_ts,comparison_timeframe FROM alerts_fired WHERE id=?1",
            [alert_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(ts, tf)| {
        crate::types::Timeframe::from_tag(&tf).and_then(|tf| ts.checked_add(tf.duration_ms()))
    }))
}

impl SqliteStore {
    pub fn ensure_user_positions_schema(&self) -> Result<()> {
        self.pool.get()?.execute_batch("CREATE TABLE IF NOT EXISTS user_positions (
            id TEXT PRIMARY KEY, symbol TEXT NOT NULL,
            side TEXT NOT NULL CHECK(side IN ('long','short')),
            entry_price REAL NOT NULL, stop_price REAL NOT NULL, target_price REAL,
            source_alert_id TEXT, created_at_ts INTEGER NOT NULL, updated_at_ts INTEGER NOT NULL,
            drawn_tf TEXT
        ); CREATE INDEX IF NOT EXISTS idx_user_positions_symbol ON user_positions(symbol,created_at_ts,id);")?;
        let c = self.pool.get()?;
        let has_anchor: bool = c
            .prepare("PRAGMA table_info(user_positions)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == "anchor_ts");
        if !has_anchor {
            c.execute(
                "ALTER TABLE user_positions ADD COLUMN anchor_ts INTEGER",
                [],
            )?;
        }
        let has_alerts: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='alerts_fired' AND type='table')",
            [],
            |r| r.get(0),
        )?;
        if has_alerts {
            let legacy = c.prepare("SELECT id,source_alert_id FROM user_positions WHERE anchor_ts IS NULL AND source_alert_id IS NOT NULL")?
                .query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, alert_id) in legacy {
                if let Some(anchor) = source_anchor(&c, &alert_id)? {
                    c.execute(
                        "UPDATE user_positions SET anchor_ts=?2 WHERE id=?1",
                        params![id, anchor],
                    )?;
                }
            }
        }
        Ok(())
    }
    pub fn list_user_positions(&self, symbol: &str) -> Result<Vec<UserPosition>> {
        let c = self.pool.get()?;
        let mut s = c.prepare(&format!(
            "SELECT {COLUMNS} FROM user_positions WHERE symbol=?1 ORDER BY created_at_ts,id"
        ))?;
        let rows = s
            .query_map([symbol], from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn create_user_position(
        &self,
        symbol: &str,
        prices: PositionPrices,
        source_alert_id: Option<String>,
        drawn_tf: Option<String>,
    ) -> Result<UserPosition> {
        prices.validate()?;
        if symbol.trim().is_empty() || symbol.len() > 200 {
            bail!("品种名称无效");
        }
        if drawn_tf
            .as_ref()
            .is_some_and(|tf| crate::types::Timeframe::from_tag(tf).is_none())
        {
            bail!("绘制周期无效");
        }
        let anchor_ts = match source_alert_id.as_deref() {
            Some(id) => source_anchor(&*self.pool.get()?, id)?,
            None => None,
        };
        let now = chrono::Utc::now().timestamp_millis();
        let p = UserPosition {
            id: format!("{:032x}", rand::random::<u128>()),
            symbol: symbol.into(),
            prices,
            source_alert_id,
            created_at_ts: now,
            updated_at_ts: now,
            drawn_tf,
            anchor_ts,
        };
        self.pool.get()?.execute("INSERT INTO user_positions (id,symbol,side,entry_price,stop_price,target_price,source_alert_id,created_at_ts,updated_at_ts,drawn_tf,anchor_ts) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![p.id,p.symbol,p.prices.side.tag(),p.prices.entry_price,p.prices.stop_price,p.prices.target_price,p.source_alert_id,now,now,p.drawn_tf,p.anchor_ts])?;
        Ok(p)
    }
    pub fn update_user_position(&self, id: &str, prices: PositionPrices) -> Result<UserPosition> {
        prices.validate()?;
        let mut c = self.pool.get()?;
        let tx = c.transaction()?;
        let mut p = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM user_positions WHERE id=?1"),
                [id],
                from_row,
            )
            .optional()?
            .context("仓位不存在，可能已删除")?;
        // Idempotent replay does not write or advance updated_at_ts.
        if p.prices != prices {
            p.updated_at_ts = chrono::Utc::now()
                .timestamp_millis()
                .max(p.updated_at_ts + 1);
            tx.execute("UPDATE user_positions SET side=?2,entry_price=?3,stop_price=?4,target_price=?5,updated_at_ts=?6 WHERE id=?1",
                params![id,prices.side.tag(),prices.entry_price,prices.stop_price,prices.target_price,p.updated_at_ts])?;
            p.prices = prices;
        }
        tx.commit()?;
        Ok(p)
    }
    pub fn delete_user_position(&self, id: &str) -> Result<()> {
        self.pool
            .get()?
            .execute("DELETE FROM user_positions WHERE id=?1", [id])?;
        Ok(())
    }

    /// Read-only draft preparation. Neither this method nor CRUD emits any event.
    pub fn user_position_draft(
        &self,
        alert_id: &str,
        decision_id: Option<&str>,
    ) -> Result<PositionDraft> {
        // Read the immutable source directly: inbox joins may hide historical alerts
        // whose candidate has been pruned, even when a complete decision snapshot exists.
        struct SourceAlert {
            id: String,
            candidate_id: String,
            smt_id: String,
            watchlist_id: String,
            validation_symbol: Option<String>,
            trade_symbols: Vec<String>,
            deterministic_score: f32,
            created_at: i64,
            validation_timeframe: String,
        }
        let alert = self.pool.get()?.query_row(
            "SELECT id,candidate_id,smt_id,COALESCE(watchlist_id,''),validation_symbol,trade_symbols,deterministic_score,created_at,validation_timeframe FROM alerts_fired WHERE id=?1",
            [alert_id], |r| {
                let symbols: String = r.get(5)?;
                let trade_symbols = serde_json::from_str(&symbols).map_err(|e| rusqlite::Error::FromSqlConversionFailure(5,rusqlite::types::Type::Text,Box::new(e)))?;
                Ok(SourceAlert { id: r.get(0)?,candidate_id: r.get(1)?,smt_id: r.get(2)?,watchlist_id: r.get(3)?,validation_symbol: r.get(4)?,trade_symbols,deterministic_score: r.get(6)?,created_at: r.get(7)?,validation_timeframe: r.get(8)? })
            }).optional()?.context("找不到来源告警")?;
        let symbol = alert
            .validation_symbol
            .clone()
            .or_else(|| (alert.trade_symbols.len() == 1).then(|| alert.trade_symbols[0].clone()))
            .context("告警交易品种不明确，不能预填")?;
        let decision = if let Some(id) = decision_id {
            Some(self.get_decision(id)?.context("找不到指定决策记录")?)
        } else {
            self.list_decisions(&alert.candidate_id)?
                .into_iter()
                .rev()
                .find(|d| {
                    d.alert_id.as_deref() == Some(alert_id)
                        && d.trade_symbol.as_deref() == Some(&symbol)
                })
        };
        let g = if let Some(d) = decision {
            if d.alert_id.as_deref() != Some(alert_id)
                || d.trade_symbol.as_deref() != Some(&symbol)
                || d.candidate_id != alert.candidate_id
                || d.watchlist_id != alert.watchlist_id
            {
                bail!("决策与来源告警不匹配");
            }
            // Deliberately never read parsed_decision_json or model output.
            let request: serde_json::Value =
                serde_json::from_str(&d.request_json).context("决策护栏快照无法读取")?;
            // LlmDecisionRequest is serde(transparent): persisted production requests
            // contain the context fields at root. Also accept explicitly wrapped snapshots.
            let value = request
                .get("deterministic_guardrails")
                .or_else(|| request.pointer("/context/deterministic_guardrails"))
                .context("该决策缺少确定性护栏快照，请手动画仓位")?;
            serde_json::from_value::<DeterministicDecisionGuardrails>(value.clone())
                .context("确定性护栏格式无效")?
        } else {
            let mut c = self.pool.get()?;
            let tx = c.transaction()?;
            let candidate_json: String = tx
                .query_row(
                    "SELECT payload_json FROM trade_candidates WHERE id=?1",
                    [&alert.candidate_id],
                    |r| r.get(0),
                )
                .context("缺少历史候选快照，请手动画仓位")?;
            let smt_json: String = tx
                .query_row(
                    "SELECT payload_json FROM smt_divergences WHERE id=?1",
                    [&alert.smt_id],
                    |r| r.get(0),
                )
                .context("缺少历史 SMT 快照，请手动画仓位")?;
            let mut candidate: CandidateSetup = serde_json::from_str(&candidate_json)?;
            let smt: SmtDivergence = serde_json::from_str(&smt_json)?;
            if candidate.smt_id != alert.smt_id
                || candidate.watchlist_id != alert.watchlist_id
                || smt.watchlist_id != alert.watchlist_id
            {
                bail!("历史快照与来源告警不匹配");
            }
            candidate.deterministic_score = alert.deterministic_score;
            for sym in [&smt.sweeper_symbol, &symbol] {
                smt.chains
                    .iter()
                    .find(|c| &c.symbol == sym)
                    .and_then(|c| c.c2_candle.as_ref())
                    .context("缺少历史 C2 快照，请手动画仓位")?;
            }
            let evidence = PackedEvidence {
                candidate_id: candidate.id.clone(),
                candidate,
                watchlist_id: alert.watchlist_id.clone(),
                alert_id: Some(alert.id.clone()),
                trade_symbol: Some(symbol.clone()),
                as_of_ts: alert.created_at,
                pda_context: smt.htf_pda_ref.clone(),
                liquidity_refs: smt.liquidity_refs.clone(),
                strength: smt.strength.clone(),
                smt,
                ltf_cisd_mss: vec![],
                context_version: M7D_CONTEXT_VERSION.into(),
                strategy_version: M7D_STRATEGY_VERSION.into(),
                market_bars: vec![],
                market_structures: vec![],
            };
            let context = pack_stored_evidence(&tx, evidence)?;
            tx.commit()?;
            context.deterministic_guardrails
        };
        let anchor_ts = source_anchor(&*self.pool.get()?, &alert.id)?;
        Ok(PositionDraft {
            symbol,
            prices: prices_from_guardrails(&g)?,
            source_alert_id: alert.id,
            drawn_tf: Some(alert.validation_timeframe),
            anchor_ts,
        })
    }
}
