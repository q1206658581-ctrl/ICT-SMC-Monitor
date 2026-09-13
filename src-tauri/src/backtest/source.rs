use super::{
    model::*,
    simulator::{simulate, simulate_exit},
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use ict_monitor::{
    candidate::{CandidateSetup, PackedEvidence},
    detector::types::SmtDivergence,
    llm::{LlmDecisionContext, M7D_CONTEXT_VERSION, M7D_STRATEGY_VERSION},
    types::{Bar, Timeframe},
};
use rusqlite::{params, Connection, OpenFlags};

pub fn open_source(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED;")?;
    Ok(conn)
}

pub fn evaluate(parameters: Parameters, run_id: String) -> Result<Evaluation> {
    let conn = open_source(std::path::Path::new(&parameters.source))?;
    // Each symbol excludes its own newest, potentially forming M1. Another
    // symbol's fresher feed must never make that candle eligible.
    let symbol_cutoffs: std::collections::BTreeMap<String, i64> = conn
        .prepare("SELECT symbol, MAX(ts) FROM bars WHERE tf='1m' GROUP BY symbol")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let cutoff = symbol_cutoffs.values().copied().max().unwrap_or(0);
    let other: usize = conn.query_row(
        "SELECT COUNT(*) FROM alerts_fired WHERE trigger NOT IN ('C2Confirmed','c2_confirmed')",
        [],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare("SELECT id,candidate_id,smt_id,COALESCE(watchlist_id,''),validation_symbol,trade_symbols,created_at,deterministic_score FROM alerts_fired WHERE trigger IN ('C2Confirmed','c2_confirmed') ORDER BY created_at,id")?;
    let mut rows = stmt.query([])?;
    let mut trades = Vec::new();
    while let Some(row) = rows.next()? {
        let symbols: String = row.get(5)?;
        let symbol: Option<String> = row.get(4)?;
        let fallback = serde_json::from_str::<Vec<String>>(&symbols)
            .ok()
            .and_then(|v| (v.len() == 1).then(|| v[0].clone()));
        let mut trade = Trade {
            alert_id: row.get(0)?,
            candidate_id: row.get(1)?,
            smt_id: row.get(2)?,
            watchlist_id: row.get(3)?,
            symbol: symbol.or(fallback).unwrap_or_default(),
            as_of_ts: row.get(6)?,
            direction: "unknown".into(),
            confidence: None,
            quality: "unknown".into(),
            session: "unknown".into(),
            weekly_slot: "unknown".into(),
            fixed_r: vec![],
            anchor_gap: None,
            user_v1: vec![],
            holding_bars: 0,
            data_issues: vec![],
            available_future_m1: 0,
            guardrails: None,
            evidence_hash: None,
            simulation: Simulation::empty(Outcome::Excluded, "not_evaluated"),
        };
        let score: f32 = row.get(7)?;
        if let Err(error) = evaluate_trade(
            &conn,
            &parameters,
            symbol_cutoffs.get(&trade.symbol).copied().unwrap_or(0),
            score,
            &mut trade,
        ) {
            trade.data_issues.push(format!("{error:#}"));
            trade.simulation = Simulation::empty(Outcome::Excluded, format!("{error:#}"));
        }
        if parameters.mode == ExitMode::FixedR && trade.fixed_r.is_empty() {
            trade.fixed_r = (1..=3)
                .map(|multiple| FixedRResult {
                    multiple,
                    simulation: trade.simulation.clone(),
                })
                .collect();
        }
        if parameters.mode == ExitMode::UserV1 && trade.user_v1.is_empty() {
            for buffer_points in USER_BUFFERS {
                for multiple in USER_MULTIPLES {
                    trade.user_v1.push(UserResult {
                        buffer_points,
                        multiple,
                        stop: None,
                        target: None,
                        target1_in_user_r: None,
                        simulation: trade.simulation.clone(),
                    });
                }
            }
        }
        if parameters.mode == ExitMode::UserV1 && trade.fixed_r.is_empty() {
            trade.fixed_r = (1..=3)
                .map(|multiple| FixedRResult {
                    multiple,
                    simulation: trade.simulation.clone(),
                })
                .collect();
        }
        trades.push(trade);
        if trades.len() % 25 == 0 {
            eprintln!("evaluated {} C2 alerts", trades.len());
        }
    }
    Ok(Evaluation {
        run_id,
        parameters,
        source_cutoff_ts: cutoff,
        symbol_cutoffs,
        c2_count: trades.len(),
        other_alert_count: other,
        trades,
    })
}

pub fn pack_evidence(conn: &Connection, evidence: PackedEvidence) -> Result<LlmDecisionContext> {
    ict_monitor::llm::pack_stored_evidence(conn, evidence)
}

fn evaluate_trade(
    conn: &Connection,
    p: &Parameters,
    cutoff: i64,
    score: f32,
    t: &mut Trade,
) -> Result<()> {
    if t.symbol.is_empty() {
        bail!("missing_unambiguous_trade_symbol")
    }
    let candidate_json: String = conn
        .query_row(
            "SELECT payload_json FROM trade_candidates WHERE id=?1",
            [&t.candidate_id],
            |r| r.get(0),
        )
        .context("missing_candidate")?;
    let smt_json: String = conn
        .query_row(
            "SELECT payload_json FROM smt_divergences WHERE id=?1",
            [&t.smt_id],
            |r| r.get(0),
        )
        .context("missing_smt")?;
    let mut candidate: CandidateSetup =
        serde_json::from_str(&candidate_json).context("malformed_candidate")?;
    let smt: SmtDivergence = serde_json::from_str(&smt_json).context("malformed_smt")?;
    if candidate.smt_id != t.smt_id
        || candidate.watchlist_id != t.watchlist_id
        || smt.watchlist_id != t.watchlist_id
    {
        bail!("source_identity_mismatch")
    }
    if !score.is_finite() {
        bail!("invalid_alert_score")
    }
    candidate.deterministic_score = score;
    // Require both authoritative C2 chains; packer's legacy fallback cannot
    // justify a historical evaluation when one side's C2 is unavailable.
    for symbol in [&smt.sweeper_symbol, &t.symbol] {
        let chain = smt
            .chains
            .iter()
            .find(|c| &c.symbol == symbol)
            .context("missing_chain")?;
        chain.c2_candle.as_ref().context("missing_c2")?;
    }
    t.holding_bars = p
        .holding_bars
        .unwrap_or(p.ttl_mtf_bars * (smt.comparison_timeframe.duration_ms() / MINUTE) as usize);
    let evidence = PackedEvidence {
        candidate_id: candidate.id.clone(),
        candidate,
        watchlist_id: t.watchlist_id.clone(),
        alert_id: Some(t.alert_id.clone()),
        trade_symbol: Some(t.symbol.clone()),
        as_of_ts: t.as_of_ts,
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
    let context = pack_evidence(conn, evidence)?;
    t.evidence_hash = Some(
        blake3::hash(&serde_json::to_vec(&context)?)
            .to_hex()
            .to_string(),
    );
    let g = context.deterministic_guardrails;
    t.direction = serde_json::to_value(g.direction)?
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    t.confidence = Some(g.confidence);
    t.quality = serde_json::to_value(g.quality)?
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    (t.session, t.weekly_slot) = time_buckets(t.as_of_ts)?;
    t.guardrails = Some(g.clone());
    if g.entry_zone.is_none() && p.mode != ExitMode::UserV1 {
        t.data_issues.push("missing_entry_zone".into());
    }
    if g.invalidation_price.is_none() && p.mode != ExitMode::UserV1 {
        t.data_issues.push("missing_invalidation".into());
    }
    if g.targets.is_empty() && p.mode == ExitMode::Targets {
        t.data_issues.push("missing_target".into());
    }
    if t.as_of_ts > cutoff {
        t.data_issues.push("anchor_after_source_cutoff".into());
    }
    let price: Option<f64> = conn.query_row(
        "SELECT (SELECT close FROM bars WHERE symbol=?1 AND tf='1m' AND ts=?2)",
        params![t.symbol, t.as_of_ts - MINUTE],
        |r| r.get(0),
    )?;
    if price.is_none() {
        t.data_issues.push("missing_anchor_m1_close".into());
        t.anchor_gap = Some(anchor_gap(
            conn,
            &t.symbol,
            t.as_of_ts,
            context.l2_strategy_reading.comparison_timeframe,
        )?);
    }
    let limit = p.entry_bars + t.holding_bars;
    let mut stmt=conn.prepare("SELECT ts,open,high,low,close,volume FROM bars WHERE symbol=?1 AND tf='1m' AND ts>=?2 AND ts+60000<=?3 ORDER BY ts LIMIT ?4")?;
    let bars = stmt
        .query_map(params![t.symbol, t.as_of_ts, cutoff, limit as i64], |r| {
            read_bar(r, &t.symbol, Timeframe::M1)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    t.available_future_m1 = bars.len();
    if p.mode == ExitMode::UserV1 && t.data_issues.is_empty() {
        let c2 = context
            .l2_strategy_reading
            .chains
            .iter()
            .find(|c| c.symbol == t.symbol)
            .and_then(|c| c.c2.as_ref())
            .context("missing_confirmed_trade_c2")?;
        t.fixed_r = (1..=3)
            .map(|multiple| FixedRResult {
                multiple,
                simulation: super::simulator::simulate_anchor(
                    &g,
                    t.as_of_ts,
                    price.unwrap(),
                    &bars,
                    t.holding_bars,
                    multiple as f64,
                ),
            })
            .collect();
        for buffer in USER_BUFFERS {
            for multiple in USER_MULTIPLES {
                t.user_v1.push(super::simulator::simulate_user(
                    &g,
                    &t.symbol,
                    c2,
                    t.as_of_ts,
                    price.unwrap(),
                    &bars,
                    t.holding_bars,
                    buffer,
                    multiple,
                ));
            }
        }
        t.simulation = Simulation::empty(Outcome::Excluded, "multi_variant_see_user_v1");
        return Ok(());
    }
    if let Some(reason) = t.data_issues.first() {
        let outcome =
            if reason.starts_with("missing_anchor") || reason == "anchor_after_source_cutoff" {
                Outcome::InsufficientData
            } else {
                Outcome::Excluded
            };
        t.simulation = Simulation::empty(outcome, reason.clone());
    } else if p.mode == ExitMode::FixedR {
        t.fixed_r = (1..=3)
            .map(|multiple| FixedRResult {
                multiple,
                simulation: simulate_exit(
                    &g,
                    t.as_of_ts,
                    price.unwrap(),
                    &bars,
                    p.entry_bars,
                    t.holding_bars,
                    Some(multiple as f64),
                ),
            })
            .collect();
        t.simulation = t.fixed_r[0].simulation.clone();
    } else {
        t.simulation = simulate(
            &g,
            t.as_of_ts,
            price.unwrap(),
            &bars,
            p.entry_bars,
            t.holding_bars,
        );
        if matches!(
            t.simulation.outcome,
            Outcome::Excluded | Outcome::DataGap | Outcome::InsufficientData
        ) {
            t.data_issues.push(t.simulation.reason.clone());
        }
    }
    Ok(())
}

fn read_bar(r: &rusqlite::Row<'_>, symbol: &str, tf: Timeframe) -> rusqlite::Result<Bar> {
    Ok(Bar {
        symbol: symbol.into(),
        tf,
        ts: r.get(0)?,
        open: r.get(1)?,
        high: r.get(2)?,
        low: r.get(3)?,
        close: r.get(4)?,
        volume: r.get(5)?,
    })
}

pub fn time_buckets(ts: i64) -> Result<(String, String)> {
    let utc = DateTime::<Utc>::from_timestamp_millis(ts).context("invalid_timestamp")?;
    let ny = utc.with_timezone(&chrono_tz::America::New_York);
    // Mutually exclusive descriptive sessions in NY local time, DST-aware.
    let session = match ny.hour() {
        20..=23 => "Asia",
        2..=4 | 10..=11 => "London",
        7..=9 => "NY",
        _ => "Off-hours",
    };
    Ok((
        session.into(),
        format!(
            "{} {:02}:00-{:02}:00 NY",
            ny.weekday(),
            ny.hour() / 4 * 4,
            ny.hour() / 4 * 4 + 4
        ),
    ))
}

fn anchor_gap(conn: &Connection, symbol: &str, as_of: i64, tf: Timeframe) -> Result<AnchorGap> {
    let expected = as_of - MINUTE;
    let (first, last): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT MIN(ts),MAX(ts) FROM bars WHERE symbol=?1 AND tf='1m'",
        [symbol],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let previous = conn.query_row(
        "SELECT MAX(ts) FROM bars WHERE symbol=?1 AND tf='1m' AND ts<?2",
        params![symbol, expected],
        |r| r.get::<_, Option<i64>>(0),
    )?;
    let next = conn.query_row(
        "SELECT MIN(ts) FROM bars WHERE symbol=?1 AND tf='1m' AND ts>?2",
        params![symbol, expected],
        |r| r.get::<_, Option<i64>>(0),
    )?;
    let local = DateTime::<Utc>::from_timestamp_millis(expected)
        .context("invalid timestamp")?
        .with_timezone(&chrono_tz::America::New_York);
    let weekend = local.weekday() == chrono::Weekday::Sat
        || local.weekday() == chrono::Weekday::Fri && local.hour() >= 17
        || local.weekday() == chrono::Weekday::Sun && local.hour() < 17;
    let category = if first.is_none() {
        "no_m1_series"
    } else if first.is_some_and(|ts| expected < ts) {
        "before_m1_coverage"
    } else if last.is_some_and(|ts| expected > ts) {
        "after_m1_coverage"
    } else if weekend {
        "possible_weekend_closure"
    } else {
        "internal_gap"
    };
    let htf: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM bars WHERE symbol=?1 AND tf=?2 AND ts=?3)",
        params![symbol, tf.tag(), as_of - tf.duration_ms()],
        |r| r.get(0),
    )?;
    Ok(AnchorGap {
        category: category.into(),
        expected_ts: expected,
        first_m1: first,
        last_m1: last,
        previous_m1: previous,
        next_m1: next,
        comparison_close_available: htf,
    })
}
