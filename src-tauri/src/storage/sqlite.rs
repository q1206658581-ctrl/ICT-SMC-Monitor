//! SQLite-backed bar store.
//!
//! Schema matches §9.1 of `docs/技术设计文档.md`. Only the `bars` table is
//! created in M1; other tables (structures, alerts_fired, llm_reports) will
//! be added in later milestones.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

use crate::alert::{AlertRecord, AlertTrigger};
use crate::candidate::{CandidateSetup, DecisionLogEntry};
use crate::detector::smt::{CURRENT_PIPELINE_VERSION, CURRENT_RULE_VERSION};
use crate::detector::types::SmtInvalidationReason;
use crate::types::Bar;

#[derive(Clone)]
pub struct SqliteStore {
    pub(super) pool: Pool<SqliteConnectionManager>,
    path: PathBuf,
}

impl SqliteStore {
    /// Open (or create) the database at `path`. Parent directory is created
    /// automatically.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db parent dir {parent:?}"))?;
        }

        let manager = SqliteConnectionManager::file(&path).with_init(|c| {
            c.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA busy_timeout=5000;",
            )
        });
        let pool = Pool::builder()
            .max_size(1)
            .build(manager)
            .context("building sqlite pool")?;

        let store = Self { pool, path };
        store.init_schema()?;
        store.ensure_user_positions_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bars (
                symbol TEXT NOT NULL,
                tf     TEXT NOT NULL,
                ts     INTEGER NOT NULL,
                open   REAL NOT NULL,
                high   REAL NOT NULL,
                low    REAL NOT NULL,
                close  REAL NOT NULL,
                volume REAL NOT NULL,
                PRIMARY KEY (symbol, tf, ts)
             );
             CREATE INDEX IF NOT EXISTS idx_bars_sym_tf
                 ON bars(symbol, tf, ts DESC);",
        )?;
        Ok(())
    }

    /// Path the store was opened at (handy for logging).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert one bar; existing (symbol, tf, ts) rows are overwritten.
    ///
    /// TradingView history can include the still-forming edge bar. If that
    /// row was cached earlier, the later finalized bar must replace it;
    /// otherwise detectors can keep structures derived from stale OHLC.
    pub fn insert_bar(&self, bar: &Bar) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO bars
                 (symbol, tf, ts, open, high, low, close, volume)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(symbol, tf, ts) DO UPDATE SET
                 open = excluded.open,
                 high = excluded.high,
                 low = excluded.low,
                 close = excluded.close,
                 volume = excluded.volume
             WHERE open != excluded.open
                OR high != excluded.high
                OR low != excluded.low
                OR close != excluded.close
                OR volume != excluded.volume",
            params![
                bar.symbol,
                bar.tf.tag(),
                bar.ts,
                bar.open,
                bar.high,
                bar.low,
                bar.close,
                bar.volume,
            ],
        )?;
        Ok(())
    }

    /// Bulk-insert a slice of bars in one transaction, overwriting existing
    /// rows when the same timestamp is re-fetched with finalized OHLC.
    pub fn insert_bars(&self, bars: &[Bar]) -> Result<usize> {
        if bars.is_empty() {
            return Ok(0);
        }
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO bars
                     (symbol, tf, ts, open, high, low, close, volume)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(symbol, tf, ts) DO UPDATE SET
                     open = excluded.open,
                     high = excluded.high,
                     low = excluded.low,
                     close = excluded.close,
                     volume = excluded.volume
                 WHERE open != excluded.open
                    OR high != excluded.high
                    OR low != excluded.low
                    OR close != excluded.close
                    OR volume != excluded.volume",
            )?;
            for bar in bars {
                inserted += stmt.execute(params![
                    bar.symbol,
                    bar.tf.tag(),
                    bar.ts,
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    bar.volume,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Insert only bars whose `(symbol, tf, ts)` key is currently absent.
    ///
    /// Reconnect repair uses complete M1 windows to fill holes left by a
    /// temporarily interrupted live aggregator. Existing provider-native
    /// higher-timeframe rows are authoritative and must not be overwritten by
    /// that repair path.
    pub fn insert_bars_if_missing(&self, bars: &[Bar]) -> Result<usize> {
        if bars.is_empty() {
            return Ok(0);
        }
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO bars
                     (symbol, tf, ts, open, high, low, close, volume)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for bar in bars {
                inserted += stmt.execute(params![
                    bar.symbol,
                    bar.tf.tag(),
                    bar.ts,
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    bar.volume,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Atomically replace ALL bars for a (symbol, tf) pair with `bars`.
    ///
    /// Used by the forex-4h re-aggregation path: TV-raw 4h bars (notably
    /// TVC:DXY) land on an inconsistent grid, so we wipe them and rewrite
    /// clean 4h bars aggregated from 1h. The live aggregator's closed-4h
    /// upserts are compatible (1h- and 1m-aggregated 4h are OHLC-identical),
    /// so there is no race beyond SQLite's serialized writes.
    pub fn replace_bars(
        &self,
        symbol: &str,
        tf: crate::types::Timeframe,
        bars: &[Bar],
    ) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        {
            tx.execute(
                "DELETE FROM bars WHERE symbol = ?1 AND tf = ?2",
                params![symbol, tf.tag()],
            )?;
            if !bars.is_empty() {
                let mut stmt = tx.prepare(
                    "INSERT INTO bars
                         (symbol, tf, ts, open, high, low, close, volume)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                for bar in bars {
                    stmt.execute(params![
                        bar.symbol,
                        bar.tf.tag(),
                        bar.ts,
                        bar.open,
                        bar.high,
                        bar.low,
                        bar.close,
                        bar.volume,
                    ])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Atomically replace a bounded timestamp slice for one symbol/TF.
    /// Used to reconcile recent locally-aggregated candles from authoritative
    /// M1 data without discarding deeper chart history fetched natively.
    pub fn replace_bars_in_range(
        &self,
        symbol: &str,
        tf: crate::types::Timeframe,
        start_ts: i64,
        end_ts: i64,
        bars: &[Bar],
    ) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM bars
              WHERE symbol = ?1 AND tf = ?2 AND ts >= ?3 AND ts <= ?4",
            params![symbol, tf.tag(), start_ts, end_ts],
        )?;
        if !bars.is_empty() {
            let mut stmt = tx.prepare(
                "INSERT INTO bars (symbol, tf, ts, open, high, low, close, volume)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for bar in bars {
                stmt.execute(params![
                    bar.symbol,
                    bar.tf.tag(),
                    bar.ts,
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    bar.volume,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Count rows for a (symbol, tf) pair — useful in smoke checks.
    pub fn count(&self, symbol: &str, tf_tag: &str) -> Result<i64> {
        let conn = self.pool.get()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM bars WHERE symbol = ?1 AND tf = ?2",
            params![symbol, tf_tag],
            |r| r.get(0),
        )?;
        Ok(n)
    }
}

impl SqliteStore {
    /// Most recent N bars (ascending by ts) for a (symbol, tf).
    /// Used by `get_history` IPC command and aggregator warm-start.
    pub fn recent_bars(
        &self,
        symbol: &str,
        tf: crate::types::Timeframe,
        limit: i64,
    ) -> Result<Vec<crate::types::Bar>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT ts, open, high, low, close, volume
             FROM bars
             WHERE symbol = ?1 AND tf = ?2
             ORDER BY ts DESC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![symbol, tf.tag(), limit])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(crate::types::Bar {
                symbol: symbol.to_string(),
                tf,
                ts: r.get(0)?,
                open: r.get(1)?,
                high: r.get(2)?,
                low: r.get(3)?,
                close: r.get(4)?,
                volume: r.get(5)?,
            });
        }
        out.reverse(); // ascending for the chart
        Ok(out)
    }

    /// Most recent N bars whose opening timestamp is at or before `max_ts`.
    /// M7 restart recovery uses this instead of a tail relative to the latest
    /// market row, so an old pending alert is rebuilt from its own as-of time.
    pub fn recent_bars_at_or_before(
        &self,
        symbol: &str,
        tf: crate::types::Timeframe,
        max_ts: i64,
        limit: i64,
    ) -> Result<Vec<crate::types::Bar>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT ts, open, high, low, close, volume
             FROM bars
             WHERE symbol = ?1 AND tf = ?2 AND ts <= ?3
             ORDER BY ts DESC LIMIT ?4",
        )?;
        let mut rows = stmt.query(params![symbol, tf.tag(), max_ts, limit])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(crate::types::Bar {
                symbol: symbol.to_string(),
                tf,
                ts: r.get(0)?,
                open: r.get(1)?,
                high: r.get(2)?,
                low: r.get(3)?,
                close: r.get(4)?,
                volume: r.get(5)?,
            });
        }
        out.reverse();
        Ok(out)
    }
}

// ---- ICT structure persistence (M3) ----------------------------------------

use crate::detector::types::IctStructure;
use crate::types::Timeframe;

impl SqliteStore {
    /// Make sure the `ict_structures` table exists. Cheap; safe to call from
    /// the engine bootstrap path. (Could be folded into `init_schema`, but
    /// keeping it separate makes the M3 layer self-contained.)
    pub fn ensure_ict_schema(&self) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ict_structures (
                id TEXT PRIMARY KEY,
                symbol TEXT NOT NULL,
                tf TEXT NOT NULL,
                kind TEXT NOT NULL,
                state TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                ts_created INTEGER NOT NULL,
                ts_updated INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_ict_sym_tf
                 ON ict_structures(symbol, tf, state);
             -- Chart refreshes merge cross-timeframe Session/Kill-Zone rows.
             -- With a long-running database, filtering those rows through the
             -- generic symbol index scans tens of thousands of unrelated
             -- structures and can starve live bar rendering during startup.
             CREATE INDEX IF NOT EXISTS idx_ict_sym_kind_state_created
                 ON ict_structures(symbol, kind, state, ts_created);
             -- Keep the regular symbol/timeframe hydration path ordered and
             -- bounded by the same fields it filters on.
             CREATE INDEX IF NOT EXISTS idx_ict_sym_tf_state_created
                 ON ict_structures(symbol, tf, state, ts_created);",
        )?;
        Ok(())
    }

    pub fn upsert_structure(&self, s: &IctStructure, now_ms: i64) -> Result<()> {
        let conn = self.pool.get()?;
        let payload = serde_json::to_string(s)?;
        conn.execute(
            "INSERT INTO ict_structures
                 (id, symbol, tf, kind, state, payload_json, ts_created, ts_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 state = excluded.state,
                 payload_json = excluded.payload_json,
                 ts_updated = excluded.ts_updated",
            params![
                s.id(),
                s.symbol(),
                s.tf().tag(),
                s.kind_tag(),
                s.state_tag(),
                payload,
                now_ms,
            ],
        )?;
        Ok(())
    }

    /// Batch-upsert all structures in a single transaction. Much faster
    /// than calling `upsert_structure` per item (avoids 30k separate
    /// auto-committed INSERTs which freeze the DB for seconds).
    pub fn upsert_structures_batch(&self, items: &[IctStructure], now_ms: i64) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO ict_structures
                     (id, symbol, tf, kind, state, payload_json, ts_created, ts_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                     state = excluded.state,
                     payload_json = excluded.payload_json,
                     ts_updated = excluded.ts_updated",
            )?;
            for s in items {
                let payload = serde_json::to_string(s)?;
                stmt.execute(params![
                    s.id(),
                    s.symbol(),
                    s.tf().tag(),
                    s.kind_tag(),
                    s.state_tag(),
                    payload,
                    now_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete every structure row whose id is NOT in `keep_ids`.
    ///
    /// Called after `upsert_structures_batch` so SQLite stays in sync
    /// with the engine's in-memory structures map. Without this,
    /// structures that were invalidated during seeding (old PO3 whose
    /// bars_waited exceeded the limit) linger in SQLite with their
    /// original state. When list_structures falls back to SQLite
    /// (engine lock held during seeding), it returns those stale rows.
    pub fn prune_structures_not_in(
        &self,
        keep_ids: &std::collections::HashSet<String>,
    ) -> Result<usize> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        let deleted = {
            tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS _prune_keep (id TEXT PRIMARY KEY)")?;
            tx.execute("DELETE FROM _prune_keep", [])?;
            {
                let mut stmt = tx.prepare("INSERT OR IGNORE INTO _prune_keep (id) VALUES (?1)")?;
                for id in keep_ids {
                    stmt.execute(params![id])?;
                }
            }
            let n = tx.execute(
                "DELETE FROM ict_structures WHERE id NOT IN (SELECT id FROM _prune_keep)",
                [],
            )?;
            tx.execute("DELETE FROM _prune_keep", [])?;
            n
        };
        tx.commit()?;
        Ok(deleted)
    }

    pub fn mark_structure_invalidated(&self, id: &str, now_ms: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE ict_structures SET state = 'invalidated', ts_updated = ?2
             WHERE id = ?1",
            params![id, now_ms],
        )?;
        Ok(())
    }

    pub fn invalidate_m4a_structures(&self, now_ms: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE ict_structures
                SET state = 'invalidated', ts_updated = ?1
              WHERE state != 'invalidated'
                AND kind IN ('liquidity_sweep', 'equal_highs_lows', 'liquidity_reversal')",
            params![now_ms],
        )?;
        Ok(n)
    }

    pub fn invalidate_m4b1_structures(&self, now_ms: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE ict_structures
                SET state = 'invalidated', ts_updated = ?1
              WHERE state != 'invalidated'
                AND kind IN ('bos', 'breaker_block', 'volume_imbalance', 'ote')",
            params![now_ms],
        )?;
        Ok(n)
    }

    pub fn invalidate_m4b2a_structures(&self, now_ms: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE ict_structures
                SET state = 'invalidated', ts_updated = ?1
              WHERE state != 'invalidated'
                AND kind IN ('premium_discount')",
            params![now_ms],
        )?;
        Ok(n)
    }

    pub fn invalidate_m4b2b_structures(&self, now_ms: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE ict_structures
                SET state = 'invalidated', ts_updated = ?1
              WHERE state != 'invalidated'
                AND kind IN ('nwog', 'ndog')",
            params![now_ms],
        )?;
        Ok(n)
    }

    pub fn invalidate_m4b2c_structures(&self, now_ms: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE ict_structures
                SET state = 'invalidated', ts_updated = ?1
              WHERE state != 'invalidated'
                AND kind IN ('session_range', 'kill_zone_window')",
            params![now_ms],
        )?;
        Ok(n)
    }

    pub fn invalidate_m4b2d_structures(&self, now_ms: i64) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE ict_structures
                SET state = 'invalidated', ts_updated = ?1
              WHERE state != 'invalidated'
                AND kind = 'power_of_3'",
            params![now_ms],
        )?;
        Ok(n)
    }

    /// Active (non-invalidated, non-mitigated) structures for hydration on
    /// startup. M4a also hydrates swept EQH/EQL and liquidity sweeps so
    /// switching TF / restarting keeps recent liquidity context visible.
    pub fn list_active_structures(&self, symbol: &str, tf: Timeframe) -> Result<Vec<IctStructure>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM (
                 SELECT payload_json, ts_created, id
                   FROM ict_structures
                  WHERE symbol = ?1
                    AND tf = ?2
                    AND state != 'invalidated'
                 UNION ALL
                 SELECT payload_json, ts_created, id
                   FROM ict_structures
                  WHERE symbol = ?1
                    AND tf != ?2
                    AND kind IN ('pdh','pdl','kill_zone','nwog','ndog','session_range','kill_zone_window')
                    AND state != 'invalidated'
             )
             ORDER BY ts_created ASC, id ASC",
        )?;
        let mut rows = stmt.query(params![symbol, tf.tag()])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let payload: String = r.get(0)?;
            match serde_json::from_str::<IctStructure>(&payload) {
                Ok(s) => out.push(s),
                Err(e) => tracing::warn!(error = ?e, "drop bad ict_structures row"),
            }
        }
        Ok(out)
    }

    /// Structure input for an as-of decision. Do not apply today's persisted
    /// `state` column here: it may describe a fill/invalidation after the old
    /// alert. The deterministic ContextPacker filters each payload by its
    /// market availability timestamp and scrubs future state transitions.
    pub fn list_structures_for_as_of(
        &self,
        symbol: &str,
        tf: Timeframe,
        _as_of_ts: i64,
    ) -> Result<Vec<IctStructure>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM ict_structures
             WHERE symbol = ?1
               AND (tf = ?2 OR kind IN ('pdh','pdl','kill_zone','nwog','ndog','session_range','kill_zone_window'))
             ORDER BY ts_created ASC, id ASC",
        )?;
        let mut rows = stmt.query(params![symbol, tf.tag()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let payload: String = row.get(0)?;
            match serde_json::from_str::<IctStructure>(&payload) {
                Ok(structure) => out.push(structure),
                Err(error) => tracing::warn!(?error, "drop bad as-of structure row"),
            }
        }
        Ok(out)
    }

    /// Session/Kill-Zone structures are calculated on M1 but displayed on
    /// every chart timeframe.  Keep this small, symbol-scoped query separate
    /// from `list_active_structures`: command handlers can merge the
    /// authoritative persisted sessions into an otherwise fresh in-memory
    /// snapshot without loading every cross-TF structure from SQLite.
    pub fn list_active_session_structures(&self, symbol: &str) -> Result<Vec<IctStructure>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM ict_structures
             WHERE symbol = ?1
               AND kind IN ('session_range', 'kill_zone_window')
               AND state != 'invalidated'
             ORDER BY ts_created ASC",
        )?;
        let mut rows = stmt.query(params![symbol])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let payload: String = r.get(0)?;
            match serde_json::from_str::<IctStructure>(&payload) {
                Ok(s) => out.push(s),
                Err(e) => tracing::warn!(error = ?e, "drop bad session structure row"),
            }
        }
        Ok(out)
    }

    /// One-shot migration to clean up the cross-weekend phantom FVG/CISD
    /// rows that the engine produced before the bar-continuity guard
    /// landed (see `engine::MAX_BAR_GAP_MULTIPLIER`). For each tracked
    /// TF, mark any non-invalidated FVG / CISD whose payload spans more
    /// than 6 bar-periods as `invalidated`. The 6× threshold is loose
    /// enough to keep legitimate three-bar structures (`ts_open` = bar
    /// i-2, `ts_confirm` = bar i, span = 2× period) intact while still
    /// catching weekend gaps (49h on M15 = 196× period). Returns the
    /// number of rows updated, summed across TFs.
    pub fn cleanup_orphaned_structures(&self, now_ms: i64) -> Result<usize> {
        // Tracked TFs that the M3 detectors emit FVG/CISD on.
        let tfs: &[Timeframe] = &[
            Timeframe::M1,
            Timeframe::M5,
            Timeframe::M15,
            Timeframe::M30,
            Timeframe::H1,
            Timeframe::H4,
            Timeframe::D1,
        ];
        let conn = self.pool.get()?;
        let mut total: usize = 0;
        for tf in tfs {
            let max_span = tf.duration_ms().saturating_mul(6);
            // FVG: ts_open / ts_confirm
            let n_fvg = conn.execute(
                "UPDATE ict_structures
                    SET state = 'invalidated', ts_updated = ?3
                  WHERE state != 'invalidated'
                    AND kind = 'fvg'
                    AND tf = ?1
                    AND (CAST(json_extract(payload_json, '$.ts_confirm') AS INTEGER)
                         - CAST(json_extract(payload_json, '$.ts_open') AS INTEGER)) > ?2",
                params![tf.tag(), max_span, now_ms],
            )?;
            // CISD: leg_origin_ts / break_ts
            let n_cisd = conn.execute(
                "UPDATE ict_structures
                    SET state = 'invalidated', ts_updated = ?3
                  WHERE state != 'invalidated'
                    AND kind = 'cisd'
                    AND tf = ?1
                    AND (CAST(json_extract(payload_json, '$.break_ts') AS INTEGER)
                         - CAST(json_extract(payload_json, '$.leg_origin_ts') AS INTEGER)) > ?2",
                params![tf.tag(), max_span, now_ms],
            )?;
            total += n_fvg + n_cisd;
            if n_fvg + n_cisd > 0 {
                tracing::info!(
                    tf = tf.tag(),
                    fvg = n_fvg,
                    cisd = n_cisd,
                    "cleanup_orphaned_structures: invalidated cross-gap rows"
                );
            }
        }
        Ok(total)
    }

    /// One-shot purge of every `state='invalidated'` row.
    ///
    /// Rationale: M3 §5 declares "no backfill on cold-start", so
    /// historical invalidated structures have no value — they can't
    /// be re-hydrated (`list_active_structures` already filters them
    /// out) and they accumulate fast enough to drown the table on
    /// every weekend gap (~1500 rows after a single Mon morning
    /// reset). Keeping them only makes ad-hoc SQL inspection
    /// confusing. Returns the number of rows deleted.
    pub fn purge_invalidated_structures(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute("DELETE FROM ict_structures WHERE state = 'invalidated'", [])?;
        Ok(n)
    }

    /// One-shot cleanup: remove seed/test data (id LIKE 'seed-%') from
    /// trade_candidates and alerts_fired. These were inserted by the
    /// now-deleted seed_test_data.sh script for manual UI testing.
    /// Safe to run on every cold start.
    pub fn purge_seed_data(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        let n_cand = conn.execute("DELETE FROM trade_candidates WHERE id LIKE 'seed-%'", [])?;
        let n_alert = conn.execute("DELETE FROM alerts_fired WHERE id LIKE 'seed-%'", [])?;
        Ok(n_cand + n_alert)
    }

    /// One-shot cleanup of leftover PDH/PDL rows. The detector's id
    /// scheme keys on `current_day_open` (the running NY day) and on
    /// `prev_open` (the just-completed day), so an older build that
    /// emitted the running-day pair as a PDH/PDL alongside the rollover
    /// pair left two pairs in the table. The active build (option A)
    /// only emits the rollover pair; clean the table on boot so users
    /// see exactly one PDH and one PDL.
    ///
    /// Implementation: simply delete every PDH/PDL row. Cold-start seed
    /// will re-emit the canonical pair from the last 1500 1m bars in the
    /// next step, persisting them via `upsert_structure`.
    pub fn purge_pdh_pdl(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "DELETE FROM ict_structures WHERE kind IN ('pdh', 'pdl')",
            [],
        )?;
        Ok(n)
    }

    /// One-time PO3 re-detection migration: delete every power_of_3 row so
    /// the next cold-start seed re-detects them from scratch with the
    /// corrected distribution-box geometry. Guarded by `user_version` so it
    /// only runs once.
    pub fn delete_all_po3(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute("DELETE FROM ict_structures WHERE kind = 'power_of_3'", [])?;
        Ok(n)
    }

    pub fn user_version(&self) -> Result<i64> {
        let conn = self.pool.get()?;
        let v: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        Ok(v)
    }

    pub fn set_user_version(&self, v: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(&format!("PRAGMA user_version = {};", v))?;
        Ok(())
    }
}

// ---- SMT divergence persistence (M5 §5) ------------------------------------

use crate::detector::types::{Direction, SmtDivergence};

impl SqliteStore {
    pub fn ensure_smt_schema(&self) -> Result<()> {
        let conn = self.pool.get()?;
        // Migrate: if old schema (has symbol_a column), drop and recreate.
        let has_old_col: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('smt_divergences')
                 WHERE name = 'symbol_a'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if has_old_col {
            tracing::info!("migrating smt_divergences: dropping old schema");
            conn.execute_batch("DROP TABLE IF EXISTS smt_divergences")?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS smt_divergences (
                id TEXT PRIMARY KEY,
                watchlist_id TEXT NOT NULL,
                sweeper_symbol TEXT NOT NULL,
                comparison_tf TEXT NOT NULL,
                direction TEXT NOT NULL,
                detection_state TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                ts_created INTEGER NOT NULL,
                ts_updated INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_smt_watchlist
                 ON smt_divergences(watchlist_id, detection_state);
             CREATE INDEX IF NOT EXISTS idx_smt_pane
                 ON smt_divergences(sweeper_symbol, comparison_tf, detection_state);",
        )?;

        // One-time reset: clear stale SMT rows that predate the
        // mtf_ref_candle field (M5 §5.8 MTF layer). replay_smt_history
        // repopulates with the new Phase A logic on startup. Guarded by
        // a detector_config marker so it runs exactly once.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS detector_config (
                detector TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                PRIMARY KEY (detector, key)
             )",
        )?;
        let reset_done: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'mtf_ref_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !reset_done {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for mtf_ref_candle migration"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'mtf_ref_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: clear stale SMT rows that predate Pattern B
        // detection (pending_refs: later bar sweeps prior swing's extreme).
        // replay_smt_history repopulates with both Pattern A and B SMTs.
        let pattern_b_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'pattern_b_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !pattern_b_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for Pattern B detection migration"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pattern_b_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: SMT ID format changed to include sweep_tf so
        // 4h-level and 1h-level SMTs for the same smt_k_ts don't collide.
        let id_v2_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'smt_id_v2_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !id_v2_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for smt_id v2 (sweep_tf in ID)"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'smt_id_v2_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: PDA reference selection changed from most-
        // significant-bar to most-recent-swing. Old SMTs have stale refs.
        let swing_ref_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'swing_ref_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !swing_ref_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for swing-based PDA ref selection"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'swing_ref_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: added bar-based fallback for PDA ref selection
        // when no swing falls inside the PDA. Previous run dropped refs
        // for SMTs without an in-PDA swing, hiding their white lines.
        let ref_fallback_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'ref_fallback_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !ref_fallback_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for ref fallback (bar-based fallback)"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'ref_fallback_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: switched to HTF-path-only detection (removed
        // MTF-path), swing-only PDA ref (no bar fallback), and counter
        // status re-computation with the PDA ref.
        let htf_only_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'htf_only_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !htf_only_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for HTF-path-only + swing-only ref"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'htf_only_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: FRACTAL_N changed from 2 to 1 (swing detection
        // is now less strict, catching more swing highs/lows).
        let fractal_n1_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'fractal_n1_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !fractal_n1_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(count = n, "resetting smt_divergences for FRACTAL_N=1");
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'fractal_n1_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // One-time reset: PDA overlap check now uses the HTF sweep bar
        // (4h) instead of the MTF SMT K (1h), so most SMTs need to be
        // regenerated with the corrected htf_pda_ref.
        let pda_htf_bar_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_htf_bar_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !pda_htf_bar_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(count = n, "resetting smt_divergences for pda_htf_bar fix");
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_htf_bar_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // One-time reset: find_pda_bar_reference now verifies close-reclaim
        // + no intermediate sweep, so references are re-selected correctly.
        let pda_ref_sweep_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_ref_sweep_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !pda_ref_sweep_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(count = n, "resetting smt_divergences for pda_ref_sweep fix");
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_ref_sweep_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // One-time reset: htf_pda_ref now cleared when no valid swing inside
        // PDA is found (prevents white lines with non-swing references).
        let pda_swing_ref_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_swing_ref_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !pda_swing_ref_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(count = n, "resetting smt_divergences for pda_swing_ref fix");
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_swing_ref_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // One-time reset: find_pda_bar_reference relaxed from close-reclaim
        // to penetration check (close-reclaim verified by original detection).
        let pda_penetration_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_penetration_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !pda_penetration_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for pda_penetration fix"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_penetration_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // One-time reset: find_pda_bar_reference reverted to strict
        // close-reclaim sweep check, and compute_htf_pda_refs now iterates
        // ALL overlapping PDAs (not just the first). Old SMT data computed
        // with the relaxed penetration check must be regenerated.
        let pda_multi_sweep_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_multi_sweep_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !pda_multi_sweep_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for pda_multi_sweep fix"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_multi_sweep_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // One-time reset: compute_htf_pda_refs now includes InvertedActive
        // and InvertedMitigated FVG states (previously only Active |
        // Mitigated50). Old SMT PDA refs must be regenerated.
        let pda_fvg_inverted_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_fvg_inverted_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !pda_fvg_inverted_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for pda_fvg_inverted fix"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_fvg_inverted_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: hard sweep validation - drop PDA (white line) for
        // any SMT whose HTF sweep bar did not actually close-reclaim the
        // sweeper reference. Clears stale invalid SMTs from prior runs so
        // the replay rebuilds everything with the validated logic.
        let sweep_hard_validate_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'sweep_hard_validate_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !sweep_hard_validate_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for hard sweep validation"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'sweep_hard_validate_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: PDA sweep-bar fix. feed_smt now uses the actual
        // sweep bar (HTF bar containing the SMT K) instead of the confirming
        // bar for the PDA overlap + close-reclaim check. Previous SMTs used
        // the wrong bar, so most Pattern-A SMTs missed their PDA. Clear all
        // so the replay rebuilds with the corrected logic.
        let pda_sweepbar_fix_reset: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_sweepbar_fix_reset'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !pda_sweepbar_fix_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(count = n, "resetting smt_divergences for PDA sweep-bar fix");
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_sweepbar_fix_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: 4H grid realignment. The 4H boundary changed from
        // NY-local 01/05/09/13/17/21 to 23/03/07/11/15/19 (matching TradingView
        // native 4H). Wipe all 4H bars so they are re-fetched from TV, and
        // wipe SMT divergences (computed on the old grid).
        let h4_grid_realign_reset: bool = conn
            .query_row(
                "SELECT value = '1' FROM detector_config
                 WHERE detector = 'smt' AND key = 'h4_grid_realign_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !h4_grid_realign_reset {
            let n = conn.execute("DELETE FROM bars WHERE tf = '4h'", [])?;
            tracing::info!(count = n, "deleting 4h bars for grid realignment");
            conn.execute("DELETE FROM smt_divergences", [])?;
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'h4_grid_realign_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: 4H grid revert. The 23/03/07/11/15/19 anchor was
        // wrong - TV native 4H uses 01/05/09/13/17/21 NY-local (the original
        // grid). Wipe 4H bars (mixed-grid leftovers) + SMT so they rebuild
        // cleanly on the correct grid.
        let h4_grid_revert_reset: bool = conn
            .query_row(
                "SELECT value = '1' FROM detector_config
                 WHERE detector = 'smt' AND key = 'h4_grid_revert_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !h4_grid_revert_reset {
            let n = conn.execute("DELETE FROM bars WHERE tf = '4h'", [])?;
            tracing::info!(count = n, "deleting 4h bars for grid revert");
            conn.execute("DELETE FROM smt_divergences", [])?;
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'h4_grid_revert_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: 4H grid chart-match. TV chart uses 23/03/07/11/15/19
        // NY-local (not the WebSocket API 01/05/09/13/17/21 grid). Re-aggregate
        // 4h from 1h on the chart grid. Wipe old 4h + SMT.
        let h4_grid_chart_match_reset: bool = conn
            .query_row(
                "SELECT value = '1' FROM detector_config
                 WHERE detector = 'smt' AND key = 'h4_grid_chart_match_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !h4_grid_chart_match_reset {
            let n = conn.execute("DELETE FROM bars WHERE tf = '4h'", [])?;
            tracing::info!(count = n, "deleting 4h bars for chart-match grid");
            conn.execute("DELETE FROM smt_divergences", [])?;
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'h4_grid_chart_match_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: FVG lifecycle changed - InvertedMitigated FVGs are
        // now kept in the engine (emit Update, not Invalidated) so they can
        // serve as PDA candidates for SMT sweeps. Also, find_pda_bar_reference
        // no longer requires the swing to be inside the PDA price band. Both
        // changes require re-detecting FVG structures and SMT divergences.
        let pda_fvg_lifecycle_reset: bool = conn
            .query_row(
                "SELECT value = '1' FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_fvg_lifecycle_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !pda_fvg_lifecycle_reset {
            let n = conn.execute("DELETE FROM ict_structures WHERE kind = 'fvg'", [])?;
            tracing::info!(count = n, "deleting all fvg structures for lifecycle reset");
            conn.execute("DELETE FROM smt_divergences", [])?;
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_fvg_lifecycle_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: PDA rules changed - InvertedMitigated FVGs removed
        // from PDA candidates, and one-PDA-one-SMT consumption mechanism
        // added. Re-detect FVGs and SMT divergences.
        let pda_consume_reset: bool = conn
            .query_row(
                "SELECT value = '1' FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_consume_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !pda_consume_reset {
            let n = conn.execute("DELETE FROM ict_structures WHERE kind = 'fvg'", [])?;
            tracing::info!(
                count = n,
                "deleting all fvg structures for pda-consume reset"
            );
            conn.execute("DELETE FROM smt_divergences", [])?;
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_consume_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }

        // One-time reset: PDA valid states narrowed to Active | Mitigated50
        // only (InvertedActive removed). Re-detect FVGs and SMT divergences.
        let pda_active_only_reset: bool = conn
            .query_row(
                "SELECT value = '1' FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_active_only_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !pda_active_only_reset {
            let n = conn.execute("DELETE FROM ict_structures WHERE kind = 'fvg'", [])?;
            tracing::info!(
                count = n,
                "deleting all fvg structures for active-only reset"
            );
            conn.execute("DELETE FROM smt_divergences", [])?;
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                VALUES ('smt', 'pda_active_only_reset', '1')
                ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // One-time reset: C2 case 1/3 tightened to SMT K only (rule m5c2.v2).
        // evaluate_c2_c3 no longer matches case 1/3 on SMT K+1; only case 2
        // (return into SMT K range) can yield C2 = SMT K+1. replay_smt_history
        // repopulates with the tightened logic.
        let c2_tighten_reset: bool = conn
            .query_row(
                "SELECT value = '1' FROM detector_config
                 WHERE detector = 'smt' AND key = 'c2_tighten_smtk_only'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !c2_tighten_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for C2 tighten (case 1/3 SMT K only)"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                VALUES ('smt', 'c2_tighten_smtk_only', '1')
                ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // One-time reset: deferred C2/C3 evaluation (rule m5c2.v3).
        // evaluate_c2_c3 now returns SmtKDetected/C2Confirmed when C2/C3
        // bars aren't closed yet, instead of Invalidated. Pending chains
        // are upgraded as MTF bars close. replay_smt_history repopulates.
        let c2_defer_reset: bool = conn
            .query_row(
                "SELECT value = '1' FROM detector_config
                 WHERE detector = 'smt' AND key = 'c2_deferred_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !c2_defer_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for deferred C2/C3 (m5c2.v3)"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'c2_deferred_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // One-time reset: counter direction flip (rule m5c2.v4).
        // C2/C3 evaluation now uses the flipped direction for inverse-
        // correlated counters (EU/GU). Old chains have wrong C2 cases.
        let c2_counter_dir_reset: bool = conn
            .query_row(
                "SELECT value = 1 FROM detector_config
                 WHERE detector = 'smt' AND key = 'c2_counter_dir_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !c2_counter_dir_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for counter direction flip (m5c2.v4)"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'c2_counter_dir_reset', 1)
                 ON CONFLICT(detector, key) DO UPDATE SET value = 1",
                [],
            )?;
        }
        // One-time reset: PDA expiry (rule m5c2.v5).
        // 4H FVGs expire after 7 days, 1H after 3 days. Old SMTs may
        // have matched stale PDAs that are now excluded.
        let pda_expiry_reset: bool = conn
            .query_row(
                "SELECT value = 1 FROM detector_config
                 WHERE detector = 'smt' AND key = 'pda_expiry_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !pda_expiry_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for PDA expiry (m5c2.v5)"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'pda_expiry_reset', 1)
                 ON CONFLICT(detector, key) DO UPDATE SET value = 1",
                [],
            )?;
        }
        // trade_symbol(String) -> trade_symbols(Vec<String>): old JSON
        // rows lack the new field and will fail deserialization, so clear
        // them once. replay_smt_history repopulates on startup.
        let trade_symbols_reset: bool = conn
            .query_row(
                "SELECT value = 1 FROM detector_config
                 WHERE detector = 'smt' AND key = 'trade_symbols_reset'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !trade_symbols_reset {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM smt_divergences", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting smt_divergences for trade_symbols migration"
                );
                conn.execute("DELETE FROM smt_divergences", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'trade_symbols_reset', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        Ok(())
    }

    /// Create the `detector_config` table if it doesn't exist (M5).
    /// Stores per-detector key/value pairs persisted across restarts so
    /// watchlist switches re-apply the user's saved thresholds to newly
    /// bootstrapped detector instances.
    pub fn ensure_detector_config_schema(&self) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS detector_config (
                detector TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (detector, key)
             );",
        )?;
        Ok(())
    }

    /// Load all rows from `detector_config`, grouped by detector name.
    /// Returns `Vec<(detector, Vec<(key, value)>)>`.
    pub fn load_detector_config(&self) -> Result<Vec<(String, Vec<(String, String)>)>> {
        let conn = self.pool.get()?;
        let mut stmt = conn
            .prepare("SELECT detector, key, value FROM detector_config ORDER BY detector, key")?;
        let mut rows = stmt.query([])?;
        let mut map: std::collections::BTreeMap<String, Vec<(String, String)>> =
            std::collections::BTreeMap::new();
        while let Some(r) = rows.next()? {
            let det: String = r.get(0)?;
            let key: String = r.get(1)?;
            let val: String = r.get(2)?;
            map.entry(det).or_default().push((key, val));
        }
        Ok(map.into_iter().collect())
    }

    /// Upsert a single detector config row.
    pub fn upsert_detector_config(&self, detector: &str, key: &str, value: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO detector_config (detector, key, value)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(detector, key) DO UPDATE SET value = excluded.value",
            params![detector, key, value],
        )?;
        Ok(())
    }

    pub fn upsert_smt(&self, s: &SmtDivergence, now_ms: i64) -> Result<()> {
        let conn = self.pool.get()?;
        let payload = serde_json::to_string(s)?;
        let direction = match s.candidate_direction {
            Direction::Bullish => "bullish",
            Direction::Bearish => "bearish",
        };
        // detection_state is per-chain (§2.2); use the sweeper chain
        // state for the column (trigger/anchor symbol).
        let det_state = s
            .chains
            .iter()
            .find(|c| c.symbol == s.sweeper_symbol)
            .map(|c| c.detection_state.tag())
            .unwrap_or("invalidated");
        conn.execute(
            "INSERT INTO smt_divergences
                 (id, watchlist_id, sweeper_symbol, comparison_tf, direction,
                 detection_state, payload_json, ts_created, ts_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(id) DO UPDATE SET
                 detection_state = excluded.detection_state,
                 payload_json = excluded.payload_json,
                 direction = excluded.direction,
                 sweeper_symbol = excluded.sweeper_symbol,
                 comparison_tf = excluded.comparison_tf,
                 ts_updated = excluded.ts_updated",
            params![
                s.id,
                s.watchlist_id,
                s.sweeper_symbol,
                s.comparison_timeframe.tag(),
                direction,
                det_state,
                payload,
                now_ms,
            ],
        )?;
        Ok(())
    }

    pub fn mark_smt_invalidated(
        &self,
        id: &str,
        now_ms: i64,
        reasons: &[SmtInvalidationReason],
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload_json FROM smt_divergences WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let invalidated_payload = payload.and_then(|payload| {
            let mut divergence = serde_json::from_str::<SmtDivergence>(&payload).ok()?;
            for chain in &mut divergence.chains {
                chain.detection_state = crate::detector::types::SmtDetectionState::Invalidated;
            }
            for reason in reasons {
                if !divergence.invalidation_reasons.contains(reason) {
                    divergence.invalidation_reasons.push(*reason);
                }
            }
            serde_json::to_string(&divergence).ok()
        });
        conn.execute(
            "UPDATE smt_divergences
                SET detection_state = 'invalidated',
                    payload_json = COALESCE(?3, payload_json),
                    ts_updated = ?2
             WHERE id = ?1",
            params![id, now_ms, invalidated_payload],
        )?;
        Ok(())
    }

    /// Mark an existing SMT audit row terminal and replace its market
    /// evidence with the canonical snapshot that caused the invalidation.
    ///
    /// `mark_smt_invalidated` intentionally preserves an old payload when a
    /// caller only has an ID (for example an inconclusive legacy audit). Live
    /// and replay detection have the full rebuilt divergence and must use this
    /// variant; otherwise the Inbox can show an old white-line reference next
    /// to a reason computed from a newer reference.
    pub fn mark_smt_invalidated_with_snapshot(
        &self,
        snapshot: &SmtDivergence,
        now_ms: i64,
        reasons: &[SmtInvalidationReason],
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let existing_payload: Option<String> = conn
            .query_row(
                "SELECT payload_json FROM smt_divergences WHERE id = ?1",
                params![snapshot.id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(existing_payload) = existing_payload else {
            // Preserve the existing contract: an SMT rejected before it was
            // ever persisted does not create a new audit row merely because
            // the terminal event was observed.
            return Ok(());
        };
        let existing = serde_json::from_str::<SmtDivergence>(&existing_payload).ok();
        let mut divergence = snapshot.clone();

        if let Some(existing) = existing {
            // PDA ownership is a formation-time fact. A C2-only invalidation
            // may not have re-resolved PDA context, so do not erase a stored
            // snapshot merely because the incoming value is absent.
            if divergence.htf_pda_ref.is_none() {
                divergence.htf_pda_ref = existing.htf_pda_ref;
            }
            if divergence.mtf_ref_candle.is_none() {
                divergence.mtf_ref_candle = existing.mtf_ref_candle;
            }
            for chain in existing.chains {
                if !divergence
                    .chains
                    .iter()
                    .any(|current| current.symbol == chain.symbol)
                {
                    divergence.chains.push(chain);
                }
            }
            for reason in existing.invalidation_reasons {
                if !divergence.invalidation_reasons.contains(&reason) {
                    divergence.invalidation_reasons.push(reason);
                }
            }
        }
        for chain in &mut divergence.chains {
            chain.detection_state = crate::detector::types::SmtDetectionState::Invalidated;
        }
        for reason in reasons {
            if !divergence.invalidation_reasons.contains(reason) {
                divergence.invalidation_reasons.push(*reason);
            }
        }
        let payload = serde_json::to_string(&divergence)?;
        conn.execute(
            "UPDATE smt_divergences
                SET detection_state = 'invalidated',
                    payload_json = ?3,
                    direction = ?4,
                    sweeper_symbol = ?5,
                    comparison_tf = ?6,
                    ts_updated = ?2
             WHERE id = ?1",
            params![
                divergence.id,
                now_ms,
                payload,
                match divergence.candidate_direction {
                    Direction::Bullish => "bullish",
                    Direction::Bearish => "bearish",
                },
                divergence.sweeper_symbol,
                divergence.comparison_timeframe.tag(),
            ],
        )?;
        Ok(())
    }

    /// Repair persisted candidates when their source SMT becomes terminal.
    /// The in-memory CandidateEngine normally performs this transition, but
    /// during cold-start replay it may not have rebuilt the candidate yet.
    /// Without this fallback the inbox can keep a stale re_check/active row
    /// whose source SMT has already been invalidated.
    pub fn mark_candidates_smt_invalidated(&self, smt_id: &str, now_ms: i64) -> Result<usize> {
        use crate::candidate::{ExpiryReason, SetupStatus};

        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        let payloads: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT payload_json FROM trade_candidates
                 WHERE smt_id = ?1
                   AND setup_status NOT IN ('invalidated', 'expired')",
            )?;
            let rows = stmt.query_map(params![smt_id], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut updated = 0usize;
        for payload in payloads {
            let mut candidate: CandidateSetup = match serde_json::from_str(&payload) {
                Ok(candidate) => candidate,
                Err(e) => {
                    tracing::warn!(error = ?e, %smt_id, "drop bad candidate during SMT invalidation repair");
                    continue;
                }
            };
            candidate.setup_status = SetupStatus::Invalidated;
            candidate.expiry_reason = Some(ExpiryReason::SmtInvalidated);
            candidate.invalidated_at = Some(now_ms);
            for symbol in candidate.trade_symbols.clone() {
                candidate.invalidate_symbol(&symbol, ExpiryReason::SmtInvalidated);
            }
            let payload = serde_json::to_string(&candidate)?;
            updated += tx.execute(
                "UPDATE trade_candidates
                    SET setup_status = 'invalidated',
                        payload_json = ?2,
                        ts_updated = ?3
                  WHERE id = ?1",
                params![candidate.id, payload, now_ms],
            )?;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Cold-start consistency repair for candidates persisted before SMT
    /// terminal propagation was implemented.
    pub fn repair_candidates_from_invalidated_smts(&self, now_ms: i64) -> Result<usize> {
        let smt_ids: Vec<String> = {
            let conn = self.pool.get()?;
            let mut stmt = conn.prepare(
                "SELECT DISTINCT c.smt_id
                   FROM trade_candidates c
                   JOIN smt_divergences s ON s.id = c.smt_id
                  WHERE c.setup_status NOT IN ('invalidated', 'expired')
                    AND s.detection_state = 'invalidated'",
            )?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut updated = 0usize;
        for smt_id in smt_ids {
            updated += self.mark_candidates_smt_invalidated(&smt_id, now_ms)?;
        }
        Ok(updated)
    }

    pub fn list_active_smt(&self, watchlist_id: &str) -> Result<Vec<SmtDivergence>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM smt_divergences
             WHERE watchlist_id = ?1 AND detection_state != 'invalidated'
             ORDER BY ts_created ASC",
        )?;
        let mut rows = stmt.query(params![watchlist_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let payload: String = r.get(0)?;
            match serde_json::from_str::<SmtDivergence>(&payload) {
                Ok(s) if s.rule_version == CURRENT_RULE_VERSION => out.push(s),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = ?e, "drop bad smt_divergences row"),
            }
        }
        Ok(out)
    }

    /// Complete SMT audit history for the Inbox. Invalidated rows retain the
    /// original candles/PDA but expose a terminal chain state to the UI.
    pub fn list_all_smt(&self, watchlist_id: &str) -> Result<Vec<SmtDivergence>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT detection_state, payload_json FROM smt_divergences
             WHERE watchlist_id = ?1
             ORDER BY ts_created ASC",
        )?;
        let mut rows = stmt.query(params![watchlist_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let state: String = row.get(0)?;
            let payload: String = row.get(1)?;
            match serde_json::from_str::<SmtDivergence>(&payload) {
                Ok(mut divergence) => {
                    if state == "invalidated" {
                        for chain in &mut divergence.chains {
                            chain.detection_state =
                                crate::detector::types::SmtDetectionState::Invalidated;
                        }
                    }
                    if divergence.rule_version == CURRENT_RULE_VERSION {
                        out.push(divergence);
                    }
                }
                Err(e) => tracing::warn!(error = ?e, "drop bad smt_divergences audit row"),
            }
        }
        Ok(out)
    }

    pub fn list_active_smt_for_pane(
        &self,
        symbol: &str,
        tf: Timeframe,
    ) -> Result<Vec<SmtDivergence>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM smt_divergences
             WHERE comparison_tf = ?1 AND detection_state != 'invalidated'
               AND sweeper_symbol = ?2
             ORDER BY ts_created ASC",
        )?;
        let mut rows = stmt.query(params![tf.tag(), symbol])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let payload: String = r.get(0)?;
            match serde_json::from_str::<SmtDivergence>(&payload) {
                Ok(s) if s.rule_version == CURRENT_RULE_VERSION => out.push(s),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = ?e, "drop bad smt_divergences row"),
            }
        }
        Ok(out)
    }

    // ---- Candidate Engine storage (M6a) ----

    /// Create `trade_candidates` + `decision_log` tables. One-time reset
    /// guarded by `detector_config` marker `candidate_v1`.
    pub fn ensure_candidate_schema(&self) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS trade_candidates (
                id TEXT PRIMARY KEY,
                rule_version TEXT NOT NULL,
                watchlist_id TEXT NOT NULL,
                smt_id TEXT NOT NULL,
                setup_type TEXT NOT NULL,
                sweeper_symbol TEXT NOT NULL,
                trade_symbols TEXT NOT NULL,
                candidate_direction TEXT NOT NULL,
                comparison_timeframe TEXT NOT NULL,
                setup_status TEXT NOT NULL,
                decision_status TEXT NOT NULL,
                deterministic_score REAL NOT NULL,
                c2_candle_ts INTEGER NOT NULL,
                c3_candle_ts INTEGER,
                validation_ts INTEGER,
                expiry_at INTEGER,
                created_at INTEGER NOT NULL,
                payload_json TEXT NOT NULL,
                ts_created INTEGER NOT NULL,
                ts_updated INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_cand_watchlist_status
                 ON trade_candidates(watchlist_id, setup_status);
             CREATE INDEX IF NOT EXISTS idx_cand_smt
                 ON trade_candidates(smt_id);
             CREATE INDEX IF NOT EXISTS idx_cand_expiry
                 ON trade_candidates(expiry_at);

             CREATE TABLE IF NOT EXISTS decision_log (
                id TEXT PRIMARY KEY,
                candidate_id TEXT NOT NULL,
                watchlist_id TEXT NOT NULL DEFAULT '',
                alert_id TEXT,
                trade_symbol TEXT,
                parent_id TEXT,
                provider TEXT NOT NULL,
                model TEXT,
                decision_mode TEXT NOT NULL,
                prompt_version TEXT,
                strategy_version TEXT NOT NULL,
                context_version TEXT NOT NULL,
                request_json TEXT NOT NULL,
                raw_response TEXT,
                parsed_decision_json TEXT NOT NULL,
                parse_ok INTEGER NOT NULL,
                error TEXT,
                created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_decision_candidate
                 ON decision_log(candidate_id);
             CREATE INDEX IF NOT EXISTS idx_decision_parent
                 ON decision_log(parent_id);",
        )?;

        // M7a additive migration: preserve all M6 deterministic history while
        // adding the routing identity required for one decision per C2 alert.
        for (col, sql_type) in [
            ("watchlist_id", "TEXT NOT NULL DEFAULT ''"),
            ("alert_id", "TEXT"),
            ("trade_symbol", "TEXT"),
        ] {
            let sql = format!(
                "SELECT COUNT(*) FROM pragma_table_info('decision_log') WHERE name = '{}'",
                col
            );
            let exists: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
            if exists == 0 {
                conn.execute(
                    &format!("ALTER TABLE decision_log ADD COLUMN {col} {sql_type}"),
                    [],
                )?;
                tracing::info!(col, "decision_log migration: added M7a column");
            }
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_decision_alert ON decision_log(alert_id);
             CREATE INDEX IF NOT EXISTS idx_decision_watchlist_created
                 ON decision_log(watchlist_id, created_at DESC);

             CREATE TABLE IF NOT EXISTS llm_daily_usage (
                 utc_day INTEGER PRIMARY KEY,
                 calls INTEGER NOT NULL
             );",
        )?;

        // Record-only strategy seeds. They document the active M7 policy and
        // never gate market facts, alerts or provider dispatch behavior.
        conn.execute(
            "INSERT INTO detector_config (detector, key, value)
             VALUES ('llm_strategy', 'm7.v1', ?1)
             ON CONFLICT(detector, key) DO UPDATE SET value = excluded.value",
            [r#"{"version":"m7.v1","strategy_version":"m7d.decision_rules.v2","mode":"single_call","notification_gate":false,"structured_context":true,"decision_owner":"deterministic_engine"}"#],
        )?;
        conn.execute(
            "INSERT INTO detector_config (detector, key, value)
             VALUES ('llm_strategy', 'm7d.decision_rules.v2', ?1)
             ON CONFLICT(detector, key) DO UPDATE SET value = excluded.value",
            [r#"{"version":"m7d.decision_rules.v2","immutable_guardrails":true,"owners":{"direction":"deterministic_engine","entry_zone":"deterministic_engine","invalidation_price":"deterministic_engine","targets":"deterministic_engine","risk_reward":"deterministic_engine","confidence":"deterministic_engine","quality":"deterministic_engine","reasoning_summary":"llm","warnings":"llm","should_wait_for":"llm"},"quality_thresholds":{"A":80,"B":65,"C":50,"Skip":0},"llm_may_decline":true,"llm_may_promote":false}"#],
        )?;

        let reset_done: bool = conn
            .query_row(
                "SELECT value = '1' FROM detector_config
                 WHERE detector = 'candidate' AND key = 'schema_v1'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !reset_done {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM trade_candidates", [], |r| r.get(0))
                .unwrap_or(0);
            if n > 0 {
                tracing::info!(
                    count = n,
                    "resetting trade_candidates for candidate_v1 migration"
                );
                conn.execute("DELETE FROM trade_candidates", [])?;
            }
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('candidate', 'schema_v1', '1')
                 ON CONFLICT(detector, key) DO UPDATE SET value = '1'",
                [],
            )?;
        }
        // Backfill the direction of the actual validation event into legacy
        // JSON snapshots. `candidate_direction` is the sweeper/SMT direction
        // and is not necessarily the direction seen on an inverse-correlated
        // trade symbol (for example DXY bullish -> EURUSD bearish).
        let has_ict_structures: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ict_structures')",
            [],
            |row| row.get(0),
        )?;
        if has_ict_structures {
            conn.execute(
                "UPDATE trade_candidates
                SET payload_json = json_set(
                    payload_json,
                    '$.validation_direction',
                    (SELECT json_extract(i.payload_json, '$.direction')
                       FROM ict_structures i
                      WHERE i.symbol = json_extract(trade_candidates.payload_json, '$.validation_symbol')
                        AND i.kind = json_extract(trade_candidates.payload_json, '$.validation_kind')
                        AND json_extract(i.payload_json, '$.break_ts') = json_extract(trade_candidates.payload_json, '$.validation_ts')
                      LIMIT 1)
                )
              WHERE json_extract(payload_json, '$.validation_ts') IS NOT NULL
                AND json_extract(payload_json, '$.validation_direction') IS NULL
                AND EXISTS (
                    SELECT 1 FROM ict_structures i
                     WHERE i.symbol = json_extract(trade_candidates.payload_json, '$.validation_symbol')
                       AND i.kind = json_extract(trade_candidates.payload_json, '$.validation_kind')
                       AND json_extract(i.payload_json, '$.break_ts') = json_extract(trade_candidates.payload_json, '$.validation_ts')
                )",
                [],
            )?;
        }
        Ok(())
    }

    pub fn upsert_candidate(&self, c: &CandidateSetup, now_ms: i64) -> Result<()> {
        let conn = self.pool.get()?;
        let payload = serde_json::to_string(c)?;
        let direction = match c.candidate_direction {
            crate::detector::types::Direction::Bullish => "bullish",
            crate::detector::types::Direction::Bearish => "bearish",
        };
        let setup_status = match c.setup_status {
            crate::candidate::SetupStatus::C2Confirmed => "c2_confirmed",
            crate::candidate::SetupStatus::Validated => "validated",
            crate::candidate::SetupStatus::ReCheck => "re_check",
            crate::candidate::SetupStatus::Expired => "expired",
            crate::candidate::SetupStatus::Invalidated => "invalidated",
        };
        let decision_status = match c.decision_status {
            crate::candidate::DecisionStatus::New => "new",
            crate::candidate::DecisionStatus::LlmPending => "llm_pending",
            crate::candidate::DecisionStatus::Approved => "approved",
            crate::candidate::DecisionStatus::Rejected => "rejected",
            crate::candidate::DecisionStatus::Expired => "expired",
        };
        let trade_symbols_json = serde_json::to_string(&c.trade_symbols)?;
        let c3_ts = c.c3_candle.as_ref().map(|c| c.ts);
        conn.execute(
            "INSERT INTO trade_candidates
                 (id, rule_version, watchlist_id, smt_id, setup_type, sweeper_symbol,
                  trade_symbols, candidate_direction, comparison_timeframe,
                  setup_status, decision_status, deterministic_score,
                  c2_candle_ts, c3_candle_ts, validation_ts, expiry_at,
                  created_at, payload_json, ts_created, ts_updated)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?19)
             ON CONFLICT(id) DO UPDATE SET
                 setup_status = excluded.setup_status,
                 decision_status = excluded.decision_status,
                 deterministic_score = excluded.deterministic_score,
                 c3_candle_ts = excluded.c3_candle_ts,
                 validation_ts = excluded.validation_ts,
                 expiry_at = excluded.expiry_at,
                 payload_json = excluded.payload_json,
                 ts_updated = excluded.ts_updated",
            params![
                c.id,
                c.rule_version,
                c.watchlist_id,
                c.smt_id,
                "smt",
                c.sweeper_symbol,
                trade_symbols_json,
                direction,
                c.comparison_timeframe.tag(),
                setup_status,
                decision_status,
                c.deterministic_score,
                c.c2_candle.ts,
                c3_ts,
                c.validation_ts,
                c.expiry_at,
                c.created_at,
                payload,
                now_ms,
            ],
        )?;
        Ok(())
    }

    pub fn list_active_candidates(&self, watchlist_id: &str) -> Result<Vec<CandidateSetup>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM trade_candidates
             WHERE watchlist_id = ?1
               AND setup_status IN ('c2_confirmed', 'validated')
             ORDER BY created_at ASC",
        )?;
        let mut rows = stmt.query(params![watchlist_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let payload: String = r.get(0)?;
            match serde_json::from_str::<CandidateSetup>(&payload) {
                Ok(c) if c.smt_rule_version == CURRENT_RULE_VERSION => out.push(c),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = ?e, "drop bad trade_candidates row"),
            }
        }
        Ok(out)
    }

    /// List all active candidates across all watchlists (for IPC list_candidates).
    /// Return ALL candidates (any status) for the inbox. The frontend's
    /// "仅展示 Validated" toggle filters between validated and the rest.
    pub fn list_all_candidates_for_inbox(&self) -> Result<Vec<CandidateSetup>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM trade_candidates
             ORDER BY created_at DESC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let payload: String = r.get(0)?;
            if let Ok(cand) = serde_json::from_str::<CandidateSetup>(&payload) {
                if cand.smt_rule_version == CURRENT_RULE_VERSION {
                    out.push(cand);
                }
            }
        }
        Ok(out)
    }

    pub fn list_all_active_candidates(&self) -> Result<Vec<CandidateSetup>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM trade_candidates
             WHERE setup_status IN ('c2_confirmed', 'validated')
             ORDER BY created_at ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let payload: String = r.get(0)?;
            if let Ok(cand) = serde_json::from_str::<CandidateSetup>(&payload) {
                if cand.smt_rule_version == CURRENT_RULE_VERSION {
                    out.push(cand);
                }
            }
        }
        Ok(out)
    }

    pub fn list_all_candidates(&self, watchlist_id: &str) -> Result<Vec<CandidateSetup>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM trade_candidates
             WHERE watchlist_id = ?1
             ORDER BY created_at ASC",
        )?;
        let mut rows = stmt.query(params![watchlist_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let payload: String = r.get(0)?;
            match serde_json::from_str::<CandidateSetup>(&payload) {
                Ok(c) if c.smt_rule_version == CURRENT_RULE_VERSION => out.push(c),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = ?e, "drop bad trade_candidates row"),
            }
        }
        Ok(out)
    }

    pub fn insert_decision(&self, d: &DecisionLogEntry) -> Result<()> {
        self.try_insert_decision(d)?;
        Ok(())
    }

    /// Atomically reserve a decision ID. `false` means another live or prior
    /// process already owns it, so callers must skip provider invocation.
    pub fn try_insert_decision(&self, d: &DecisionLogEntry) -> Result<bool> {
        let conn = self.pool.get()?;
        let decision_mode = match d.decision_mode {
            crate::candidate::DecisionMode::Deterministic => "deterministic",
            crate::candidate::DecisionMode::LlmSingleCall => "llm_single_call",
            crate::candidate::DecisionMode::MultiAgent => "multi_agent",
        };
        let changed = conn.execute(
            "INSERT INTO decision_log
                 (id, candidate_id, watchlist_id, alert_id, trade_symbol,
                  parent_id, provider, model, decision_mode,
                  prompt_version, strategy_version, context_version,
                  request_json, raw_response, parsed_decision_json,
                  parse_ok, error, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
             ON CONFLICT(id) DO NOTHING",
            params![
                d.id,
                d.candidate_id,
                d.watchlist_id,
                d.alert_id,
                d.trade_symbol,
                d.parent_id,
                d.provider,
                d.model,
                decision_mode,
                d.prompt_version,
                d.strategy_version,
                d.context_version,
                d.request_json,
                d.raw_response,
                d.parsed_decision_json,
                d.parse_ok,
                d.error,
                d.created_at,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Complete a previously reserved M7 decision row. Routing identity and
    /// ID remain immutable; only strategy/provider output is updated.
    pub fn update_decision(&self, d: &DecisionLogEntry) -> Result<()> {
        let conn = self.pool.get()?;
        let decision_mode = match d.decision_mode {
            crate::candidate::DecisionMode::Deterministic => "deterministic",
            crate::candidate::DecisionMode::LlmSingleCall => "llm_single_call",
            crate::candidate::DecisionMode::MultiAgent => "multi_agent",
        };
        conn.execute(
            "UPDATE decision_log SET
                 provider=?2, model=?3, decision_mode=?4, prompt_version=?5,
                 strategy_version=?6, context_version=?7, request_json=?8,
                 raw_response=?9, parsed_decision_json=?10, parse_ok=?11,
                 error=?12, created_at=?13
             WHERE id=?1",
            params![
                d.id,
                d.provider,
                d.model,
                decision_mode,
                d.prompt_version,
                d.strategy_version,
                d.context_version,
                d.request_json,
                d.raw_response,
                d.parsed_decision_json,
                d.parse_ok,
                d.error,
                d.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_decision(&self, id: &str) -> Result<Option<DecisionLogEntry>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, candidate_id, watchlist_id, alert_id, trade_symbol,
                    parent_id, provider, model, decision_mode,
                    prompt_version, strategy_version, context_version,
                    request_json, raw_response, parsed_decision_json,
                    parse_ok, error, created_at
             FROM decision_log WHERE id = ?1",
            [id],
            decision_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Rows reserved by a previous live process that never reached a
    /// provider result. Completed parse failures/provider errors are not
    /// retried automatically: only this exact all-empty output shape is a
    /// crash/interruption marker.
    pub fn list_pending_llm_decisions(&self) -> Result<Vec<DecisionLogEntry>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, candidate_id, watchlist_id, alert_id, trade_symbol,
                    parent_id, provider, model, decision_mode,
                    prompt_version, strategy_version, context_version,
                    request_json, raw_response, parsed_decision_json,
                    parse_ok, error, created_at
               FROM decision_log
              WHERE decision_mode IN ('single_call', 'llm_single_call')
                AND parse_ok = 0
                AND raw_response IS NULL
                AND error IS NULL
              ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], decision_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Atomically reserve one logical LLM decision call for a UTC day.
    /// Provider retries belong to that same logical call and are not counted
    /// again. A new UTC day naturally creates a fresh row.
    pub fn reserve_llm_daily_call(&self, now_ms: i64, max_calls: u32) -> Result<bool> {
        let utc_day = now_ms.div_euclid(86_400_000);
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        let current: i64 = tx
            .query_row(
                "SELECT calls FROM llm_daily_usage WHERE utc_day = ?1",
                [utc_day],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if current >= i64::from(max_calls) {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO llm_daily_usage (utc_day, calls) VALUES (?1, 1)
             ON CONFLICT(utc_day) DO UPDATE SET calls = calls + 1",
            [utc_day],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn list_decisions(&self, candidate_id: &str) -> Result<Vec<DecisionLogEntry>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, candidate_id, watchlist_id, alert_id, trade_symbol,
                    parent_id, provider, model, decision_mode,
                    prompt_version, strategy_version, context_version,
                    request_json, raw_response, parsed_decision_json,
                    parse_ok, error, created_at
             FROM decision_log
             WHERE candidate_id = ?1
             ORDER BY created_at ASC",
        )?;
        let mut rows = stmt.query(params![candidate_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(decision_from_row(r)?);
        }
        Ok(out)
    }

    /// Product inbox source for M7d. Only LLM sidecar rows are returned;
    /// deterministic M6 audit decisions remain private to candidate history.
    pub fn list_llm_decisions_for_watchlist(
        &self,
        watchlist_id: &str,
    ) -> Result<Vec<DecisionLogEntry>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, candidate_id, watchlist_id, alert_id, trade_symbol,
                    parent_id, provider, model, decision_mode,
                    prompt_version, strategy_version, context_version,
                    request_json, raw_response, parsed_decision_json,
                    parse_ok, error, created_at
             FROM decision_log
             WHERE watchlist_id = ?1
               AND decision_mode IN ('single_call', 'llm_single_call')
             ORDER BY created_at DESC, trade_symbol ASC, alert_id ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![watchlist_id], decision_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

fn decision_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DecisionLogEntry> {
    let decision_mode_str: String = row.get(8)?;
    let decision_mode = match decision_mode_str.as_str() {
        "deterministic" => crate::candidate::DecisionMode::Deterministic,
        "single_call" | "llm_single_call" => crate::candidate::DecisionMode::LlmSingleCall,
        "multi_agent" => crate::candidate::DecisionMode::MultiAgent,
        _ => crate::candidate::DecisionMode::Deterministic,
    };
    Ok(DecisionLogEntry {
        id: row.get(0)?,
        candidate_id: row.get(1)?,
        watchlist_id: row.get(2)?,
        alert_id: row.get(3)?,
        trade_symbol: row.get(4)?,
        parent_id: row.get(5)?,
        provider: row.get(6)?,
        model: row.get(7)?,
        decision_mode,
        prompt_version: row.get(9)?,
        strategy_version: row.get(10)?,
        context_version: row.get(11)?,
        request_json: row.get(12)?,
        raw_response: row.get(13)?,
        parsed_decision_json: row.get(14)?,
        parse_ok: row.get::<_, i64>(15)? != 0,
        error: row.get(16)?,
        created_at: row.get(17)?,
    })
}

impl SqliteStore {
    // ---- Alert (M6b) ----

    /// Create alerts_fired table (one-time, marker `alert_v1`).
    pub fn ensure_alert_schema(&self) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS detector_config (
                detector TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                PRIMARY KEY (detector, key)
             )",
        )?;
        let done: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM detector_config WHERE detector = 'alert' AND key = 'schema_v1'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if done == 0 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS alerts_fired (
                    id TEXT PRIMARY KEY,
                    candidate_id TEXT NOT NULL,
                    smt_id TEXT NOT NULL,
                    rule_version TEXT NOT NULL,
                    trigger TEXT NOT NULL,
                    symbol_set TEXT NOT NULL,
                    sweeper_symbol TEXT NOT NULL,
                    trade_symbols TEXT NOT NULL,
                    candidate_direction TEXT NOT NULL,
                    deterministic_score REAL NOT NULL,
                    c2_case INTEGER,
                    context_timeframe TEXT,
                    comparison_timeframe TEXT,
                    validation_timeframe TEXT,
                    c2_candle_ts INTEGER,
                    c3_candle_ts INTEGER,
                    smt_k_candle_ts INTEGER,
                    context_pda_id TEXT,
                    validation_kind TEXT,
                    validation_symbol TEXT,
                    validation_ts INTEGER,
                    channels_fired TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_alerts_candidate ON alerts_fired(candidate_id);
                CREATE INDEX IF NOT EXISTS idx_alerts_created ON alerts_fired(created_at DESC);",
            )?;
            conn.execute(
                "INSERT INTO detector_config (detector, key, value) VALUES ('alert', 'schema_v1', '1')",
                [],
            )?;
            tracing::info!("alerts_fired table created (alert_v1)");
        }
        // Migration: add TF columns if missing (added after initial v1).
        for (col, sql_type) in [
            ("context_timeframe", "TEXT"),
            ("comparison_timeframe", "TEXT"),
            ("validation_timeframe", "TEXT"),
            ("validation_direction", "TEXT"),
            ("smt_k_candle_ts", "INTEGER"),
            ("context_pda_id", "TEXT"),
            ("watchlist_id", "TEXT"),
        ] {
            let sql = format!(
                "SELECT COUNT(*) FROM pragma_table_info('alerts_fired') WHERE name = '{}'",
                col
            );
            let exists: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
            if exists == 0 {
                conn.execute(
                    &format!("ALTER TABLE alerts_fired ADD COLUMN {col} {sql_type}"),
                    [],
                )?;
                tracing::info!(col, "alerts_fired migration: added column");
            }
        }
        let has_ict_structures: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ict_structures')",
            [],
            |row| row.get(0),
        )?;
        if has_ict_structures {
            conn.execute(
                "UPDATE alerts_fired
                SET validation_direction = (
                    SELECT json_extract(i.payload_json, '$.direction')
                      FROM ict_structures i
                     WHERE i.symbol = alerts_fired.validation_symbol
                       AND i.kind = alerts_fired.validation_kind
                       AND json_extract(i.payload_json, '$.break_ts') = alerts_fired.validation_ts
                     LIMIT 1
                )
              WHERE validation_ts IS NOT NULL
                AND validation_direction IS NULL",
                [],
            )?;
            conn.execute(
                "UPDATE alerts_fired
                    SET watchlist_id = COALESCE(
                        NULLIF(watchlist_id, ''),
                        (SELECT json_extract(c.payload_json, '$.watchlist_id')
                           FROM trade_candidates c
                          WHERE c.id = alerts_fired.candidate_id)
                    )
                  WHERE watchlist_id IS NULL OR watchlist_id = ''",
                [],
            )?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_alerts_watchlist_created
                ON alerts_fired(watchlist_id, created_at DESC);",
        )?;
        let has_candidates: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='trade_candidates')",
            [],
            |row| row.get(0),
        )?;
        if has_candidates {
            // Backfill funnel audit fields for alerts created before they
            // were stored directly on AlertRecord.
            conn.execute(
                "UPDATE alerts_fired
                    SET smt_k_candle_ts = COALESCE(
                            smt_k_candle_ts,
                            (SELECT json_extract(c.payload_json, '$.smt_k_candle.ts')
                               FROM trade_candidates c
                              WHERE c.id = alerts_fired.candidate_id)
                        ),
                        context_pda_id = COALESCE(
                            context_pda_id,
                            (SELECT json_extract(c.payload_json, '$.context_pda_id')
                               FROM trade_candidates c
                              WHERE c.id = alerts_fired.candidate_id)
                        )",
                [],
            )?;
        }
        // Alert Inbox is keyed by each trade symbol's own C2 confirmation.
        // Older builds used max(trade C2, sweeper C2), which could delay the
        // row by one comparison candle. c2_candle_ts stores the trade C2 bar
        // open, so add its comparison-TF duration to recover the confirmation
        // boundary (for example M30 08:00 -> 08:30).
        conn.execute(
            "UPDATE alerts_fired
                SET created_at = c2_candle_ts + CASE comparison_timeframe
                    WHEN '5m' THEN 300000
                    WHEN '30m' THEN 1800000
                    WHEN '1h' THEN 3600000
                    ELSE 0
                END
              WHERE trigger IN ('C2Confirmed', 'c2_confirmed')
                AND c2_candle_ts IS NOT NULL",
            [],
        )?;
        Ok(())
    }

    /// One-time derived-data rebuild when the public SMT formation contract
    /// changes. Raw market bars are always preserved. The EU/GU/DXY H1/H4
    /// structure projections are also cleared because startup canonicalizes
    /// those parent candles from retained M1 history before detector replay;
    /// retaining structures detected on an older parent grid would let an
    /// impossible FVG/PDA re-enter the new SMT pipeline during rehydration.
    pub fn reset_derived_smt_pipeline_for_current_rule(&self) -> Result<usize> {
        let mut conn = self.pool.get()?;
        let current: Option<String> = conn
            .query_row(
                "SELECT value FROM detector_config
                  WHERE detector = 'smt' AND key = 'derived_rule_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if current.as_deref() == Some(CURRENT_PIPELINE_VERSION) {
            return Ok(0);
        }

        let tx = conn.transaction()?;
        let mut removed = 0usize;
        // Downstream first keeps the operation valid if foreign keys are
        // introduced later. These tables currently store only derived facts.
        removed += tx.execute("DELETE FROM alerts_fired", [])?;
        removed += tx.execute("DELETE FROM decision_log", [])?;
        removed += tx.execute("DELETE FROM trade_candidates", [])?;
        removed += tx.execute("DELETE FROM smt_divergences", [])?;
        removed += tx.execute(
            "DELETE FROM ict_structures
              WHERE tf IN ('1h', '4h')
                AND symbol IN ('TVC:DXY', 'OANDA:EURUSD', 'OANDA:GBPUSD')",
            [],
        )?;
        tx.execute(
            "INSERT INTO detector_config (detector, key, value)
             VALUES ('smt', 'derived_rule_version', ?1)
             ON CONFLICT(detector, key) DO UPDATE SET value = excluded.value",
            params![CURRENT_PIPELINE_VERSION],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    pub fn insert_alert(&self, a: &AlertRecord) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR REPLACE INTO alerts_fired
             (id, watchlist_id, candidate_id, smt_id, rule_version, trigger, symbol_set,
              sweeper_symbol, trade_symbols, candidate_direction,
              deterministic_score, c2_case, context_timeframe,
              comparison_timeframe, validation_timeframe,
              c2_candle_ts, c3_candle_ts, smt_k_candle_ts, context_pda_id,
              validation_kind, validation_symbol, validation_ts,
              validation_direction, channels_fired, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
            params![
                a.id,
                a.watchlist_id,
                a.candidate_id,
                a.smt_id,
                a.rule_version,
                match a.trigger {
                    AlertTrigger::C2Confirmed => "C2Confirmed",
                    AlertTrigger::Validated => "Validated",
                },
                serde_json::to_string(&a.symbol_set)?,
                a.sweeper_symbol,
                serde_json::to_string(&a.trade_symbols)?,
                format!("{:?}", a.candidate_direction).to_lowercase(),
                a.deterministic_score,
                a.c2_case,
                a.context_timeframe.tag(),
                a.comparison_timeframe.tag(),
                a.validation_timeframe.tag(),
                a.c2_candle_ts,
                a.c3_candle_ts,
                a.smt_k_candle_ts,
                a.context_pda_id,
                a.validation_kind.map(|k| format!("{:?}", k).to_lowercase()),
                a.validation_symbol,
                a.validation_ts,
                a.validation_direction.map(|d| format!("{:?}", d).to_lowercase()),
                serde_json::to_string(&a.channels_fired)?,
                a.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_alerts(&self, limit: Option<i64>) -> Result<Vec<AlertRecord>> {
        let conn = self.pool.get()?;
        let has_candidates: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='trade_candidates')",
            [],
            |row| row.get(0),
        )?;
        let sql = match (limit, has_candidates) {
            (Some(_), true) => {
                "SELECT a.*, COALESCE(c.setup_status, 'validated') AS current_setup_status
                        FROM alerts_fired a
                        JOIN trade_candidates c ON c.id = a.candidate_id
                        WHERE json_extract(c.payload_json, '$.smt_rule_version') = ?1
                        ORDER BY a.created_at DESC LIMIT ?2"
            }
            (None, true) => {
                "SELECT a.*, COALESCE(c.setup_status, 'validated') AS current_setup_status
                     FROM alerts_fired a
                     JOIN trade_candidates c ON c.id = a.candidate_id
                     WHERE json_extract(c.payload_json, '$.smt_rule_version') = ?1
                     ORDER BY a.created_at DESC"
            }
            (Some(_), false) => {
                "SELECT a.*, 'validated' AS current_setup_status
                   FROM alerts_fired a ORDER BY a.created_at DESC LIMIT ?1"
            }
            (None, false) => {
                "SELECT a.*, 'validated' AS current_setup_status
                   FROM alerts_fired a ORDER BY a.created_at DESC"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<AlertRecord> {
            let symbol_set_str: String = r.get("symbol_set")?;
            let trade_symbols_str: String = r.get("trade_symbols")?;
            let channels_str: String = r.get("channels_fired")?;
            let dir_str: String = r.get("candidate_direction")?;
            let trigger_str: String = r.get("trigger")?;
            let val_kind_str: Option<String> = r.get("validation_kind")?;
            let val_dir_str: Option<String> = r.get("validation_direction")?;
            Ok(AlertRecord {
                id: r.get("id")?,
                watchlist_id: r
                    .get::<_, Option<String>>("watchlist_id")?
                    .unwrap_or_default(),
                candidate_id: r.get("candidate_id")?,
                smt_id: r.get("smt_id")?,
                rule_version: r.get("rule_version")?,
                trigger: match trigger_str.as_str() {
                    "C2Confirmed" | "c2_confirmed" => AlertTrigger::C2Confirmed,
                    _ => AlertTrigger::Validated,
                },
                setup_status: match r.get::<_, String>("current_setup_status")?.as_str() {
                    "c2_confirmed" => crate::candidate::SetupStatus::C2Confirmed,
                    "re_check" => crate::candidate::SetupStatus::ReCheck,
                    "expired" => crate::candidate::SetupStatus::Expired,
                    "invalidated" => crate::candidate::SetupStatus::Invalidated,
                    _ => crate::candidate::SetupStatus::Validated,
                },
                invalidation_reason: None,
                symbol_set: serde_json::from_str(&symbol_set_str).unwrap_or_default(),
                sweeper_symbol: r.get("sweeper_symbol")?,
                trade_symbols: serde_json::from_str(&trade_symbols_str).unwrap_or_default(),
                candidate_direction: match dir_str.as_str() {
                    "bullish" => crate::detector::types::Direction::Bullish,
                    _ => crate::detector::types::Direction::Bearish,
                },
                deterministic_score: r.get("deterministic_score")?,
                c2_case: r.get("c2_case")?,
                context_timeframe: {
                    let s: String = r.get("context_timeframe").unwrap_or_default();
                    crate::types::Timeframe::from_tag(&s).unwrap_or(crate::types::Timeframe::M5)
                },
                comparison_timeframe: {
                    let s: String = r.get("comparison_timeframe").unwrap_or_default();
                    crate::types::Timeframe::from_tag(&s).unwrap_or(crate::types::Timeframe::M5)
                },
                validation_timeframe: {
                    let s: String = r.get("validation_timeframe").unwrap_or_default();
                    crate::types::Timeframe::from_tag(&s).unwrap_or(crate::types::Timeframe::M5)
                },
                c2_candle_ts: r.get("c2_candle_ts")?,
                c3_candle_ts: r.get("c3_candle_ts")?,
                smt_k_candle_ts: r.get("smt_k_candle_ts")?,
                context_pda_id: r.get("context_pda_id")?,
                validation_kind: val_kind_str.map(|s| match s.as_str() {
                    "cisd" => crate::detector::types::ReversalConfirmKind::Cisd,
                    _ => crate::detector::types::ReversalConfirmKind::Mss,
                }),
                validation_symbol: r.get("validation_symbol")?,
                validation_ts: r.get("validation_ts")?,
                validation_direction: val_dir_str.map(|s| match s.as_str() {
                    "bullish" => crate::detector::types::Direction::Bullish,
                    _ => crate::detector::types::Direction::Bearish,
                }),
                channels_fired: serde_json::from_str(&channels_str).unwrap_or_default(),
                created_at: r.get("created_at")?,
            })
        };
        let bind_values: Vec<rusqlite::types::Value> = match (limit, has_candidates) {
            (Some(n), true) => vec![CURRENT_RULE_VERSION.to_string().into(), n.into()],
            (None, true) => vec![CURRENT_RULE_VERSION.to_string().into()],
            (Some(n), false) => vec![n.into()],
            (None, false) => Vec::new(),
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(bind_values), map_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn has_fired_for_candidate(&self, candidate_id: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM alerts_fired
              WHERE candidate_id = ?1 AND trigger = 'Validated'",
            params![candidate_id],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Trigger-specific records use deterministic IDs, so this is the
    /// strongest dedupe guard and does not let a C2 alert suppress a later
    /// reversal record for the same symbol.
    pub fn has_alert_id(&self, alert_id: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM alerts_fired WHERE id = ?1",
            params![alert_id],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn has_fired_for_candidate_symbol(
        &self,
        candidate_id: &str,
        validation_symbol: &str,
    ) -> Result<bool> {
        let conn = self.pool.get()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM alerts_fired
              WHERE candidate_id = ?1
                AND validation_symbol = ?2
                AND trigger = 'Validated'",
            params![candidate_id, validation_symbol],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Group-scoped candidate idempotency. Candidate IDs are expected to be
    /// group-unique, but keeping the group predicate here prevents legacy or
    /// imported rows from suppressing another strategy group.
    pub fn has_fired_for_candidate_group_symbol(
        &self,
        candidate_id: &str,
        watchlist_id: &str,
        validation_symbol: &str,
    ) -> Result<bool> {
        let conn = self.pool.get()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM alerts_fired
              WHERE candidate_id = ?1
                AND watchlist_id = ?2
                AND validation_symbol = ?3
                AND trigger = 'Validated'",
            params![candidate_id, watchlist_id, validation_symbol],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Idempotency across internal SMT/Candidate retries of the same public
    /// PDA + HTF-liquidity episode.
    pub fn has_fired_for_candidate_episode_symbol(
        &self,
        candidate: &CandidateSetup,
        validation_symbol: &str,
    ) -> Result<bool> {
        let Some(pda_id) = candidate.context_pda_id.as_deref() else {
            return self.has_fired_for_candidate_symbol(&candidate.id, validation_symbol);
        };
        let conn = self.pool.get()?;
        let direction = format!("{:?}", candidate.candidate_direction).to_lowercase();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*)
               FROM alerts_fired a
               JOIN trade_candidates c ON c.id = a.candidate_id
              WHERE a.validation_symbol = ?1
                AND a.trigger = 'Validated'
                AND a.context_pda_id = ?2
                AND a.context_timeframe = ?3
                AND a.comparison_timeframe = ?4
                AND a.candidate_direction = ?5
                AND json_extract(c.payload_json, '$.watchlist_id') = ?6
                AND json_extract(c.payload_json, '$.observation_window[0]') = ?7",
            params![
                validation_symbol,
                pda_id,
                candidate.context_timeframe.tag(),
                candidate.comparison_timeframe.tag(),
                direction,
                candidate.watchlist_id,
                candidate.observation_window.0,
            ],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn clear_alerts(&self) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM alerts_fired", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Timeframe;

    fn temp_db_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("ict-monitor-{name}-{unique}.db"))
    }

    fn bar(close: f64) -> Bar {
        Bar {
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M5,
            ts: 1_000,
            open: 1.0,
            high: 1.2,
            low: 0.9,
            close,
            volume: 1.0,
        }
    }

    #[test]
    fn insert_bar_overwrites_edge_bar_values() {
        let path = temp_db_path("upsert-bar");
        let store = SqliteStore::open(&path).expect("open test db");

        store.insert_bar(&bar(1.15)).expect("insert stale edge bar");
        store.insert_bar(&bar(1.05)).expect("insert finalized bar");

        let rows = store
            .recent_bars("OANDA:EURUSD", Timeframe::M5, 1)
            .expect("query bars");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].close, 1.05);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn insert_bars_if_missing_preserves_existing_native_values() {
        let path = temp_db_path("insert-bars-if-missing");
        let store = SqliteStore::open(&path).expect("open test db");

        store.insert_bar(&bar(1.15)).expect("insert native bar");
        let inserted = store
            .insert_bars_if_missing(&[bar(1.05)])
            .expect("additive repair");

        assert_eq!(inserted, 0);
        let rows = store
            .recent_bars("OANDA:EURUSD", Timeframe::M5, 1)
            .expect("query bars");
        assert_eq!(rows[0].close, 1.15);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn replace_bars_wipes_and_rewrites_pair() {
        let path = temp_db_path("replace-bars");
        let store = SqliteStore::open(&path).expect("open test db");

        // Dirty 4h bars (off-grid timestamps) for DXY.
        let dirty = vec![
            Bar {
                symbol: "TVC:DXY".into(),
                tf: Timeframe::H4,
                ts: 100,
                open: 1.0,
                high: 1.1,
                low: 0.9,
                close: 1.05,
                volume: 1.0,
            },
            Bar {
                symbol: "TVC:DXY".into(),
                tf: Timeframe::H4,
                ts: 200,
                open: 1.0,
                high: 1.1,
                low: 0.9,
                close: 1.05,
                volume: 1.0,
            },
        ];
        store.insert_bars(&dirty).expect("insert dirty bars");
        assert_eq!(store.count("TVC:DXY", Timeframe::H4.tag()).unwrap(), 2);

        // Clean replacement on the canonical grid.
        let clean = vec![Bar {
            symbol: "TVC:DXY".into(),
            tf: Timeframe::H4,
            ts: 1000,
            open: 2.0,
            high: 2.2,
            low: 1.9,
            close: 2.1,
            volume: 5.0,
        }];
        store
            .replace_bars("TVC:DXY", Timeframe::H4, &clean)
            .expect("replace bars");

        // Dirty bars gone; only the clean one remains.
        assert_eq!(store.count("TVC:DXY", Timeframe::H4.tag()).unwrap(), 1);
        let rows = store
            .recent_bars("TVC:DXY", Timeframe::H4, 10)
            .expect("query bars");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts, 1000);
        assert_eq!(rows[0].close, 2.1);

        // Other (symbol, tf) pairs are untouched, and empty replace clears all.
        store
            .insert_bars(&[Bar {
                symbol: "OANDA:EURUSD".into(),
                tf: Timeframe::H1,
                ts: 50,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 0.0,
            }])
            .unwrap();
        store
            .replace_bars("TVC:DXY", Timeframe::H4, &[])
            .expect("replace with empty");
        assert_eq!(store.count("TVC:DXY", Timeframe::H4.tag()).unwrap(), 0);
        assert_eq!(store.count("OANDA:EURUSD", Timeframe::H1.tag()).unwrap(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn active_session_query_is_symbol_scoped_and_excludes_invalidated_rows() {
        use crate::detector::types::{IctStructure, KillZoneWindow, SessionKind, SessionRange};

        let path = temp_db_path("active-sessions");
        let store = SqliteStore::open(&path).expect("open test db");
        store.ensure_ict_schema().expect("ict schema");

        let range = IctStructure::SessionRange(SessionRange {
            id: "dxy-session-active".into(),
            symbol: "TVC:DXY".into(),
            tf: Timeframe::M1,
            session: SessionKind::Asia,
            label: "Asia".into(),
            ts_start: 1_000,
            ts_end: 2_000,
            high: 100.0,
            low: 99.0,
            high_ts: 1_200,
            low_ts: 1_400,
            finalized: true,
        });
        let window = IctStructure::KillZoneWindow(KillZoneWindow {
            id: "dxy-window-active".into(),
            symbol: "TVC:DXY".into(),
            tf: Timeframe::M1,
            session: SessionKind::Asia,
            label: "Asia".into(),
            ts_start: 1_000,
            ts_end: 2_000,
        });
        let invalidated = IctStructure::SessionRange(SessionRange {
            id: "dxy-session-invalidated".into(),
            symbol: "TVC:DXY".into(),
            tf: Timeframe::M1,
            session: SessionKind::LondonOpen,
            label: "LO".into(),
            ts_start: 3_000,
            ts_end: 4_000,
            high: 101.0,
            low: 100.0,
            high_ts: 3_200,
            low_ts: 3_400,
            finalized: true,
        });
        let other_symbol = IctStructure::SessionRange(SessionRange {
            id: "eu-session-active".into(),
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M1,
            session: SessionKind::Asia,
            label: "Asia".into(),
            ts_start: 1_000,
            ts_end: 2_000,
            high: 1.1,
            low: 1.0,
            high_ts: 1_200,
            low_ts: 1_400,
            finalized: true,
        });
        for structure in [&range, &window, &invalidated, &other_symbol] {
            store
                .upsert_structure(structure, 5_000)
                .expect("upsert structure");
        }
        store
            .mark_structure_invalidated(invalidated.id(), 6_000)
            .expect("invalidate structure");

        let rows = store
            .list_active_session_structures("TVC:DXY")
            .expect("query sessions");
        let ids: std::collections::HashSet<&str> =
            rows.iter().map(|structure| structure.id()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("dxy-session-active"));
        assert!(ids.contains("dxy-window-active"));
        assert!(!ids.contains("dxy-session-invalidated"));
        assert!(!ids.contains("eu-session-active"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn current_smt_rule_reset_clears_pipeline_and_core_fx_htf_structures() {
        let path = temp_db_path("smt-rule-reset");
        let store = SqliteStore::open(&path).expect("open test db");
        store
            .ensure_detector_config_schema()
            .expect("detector config schema");
        store.ensure_ict_schema().expect("ict schema");
        store.ensure_smt_schema().expect("smt schema");
        store.ensure_candidate_schema().expect("candidate schema");
        store.ensure_alert_schema().expect("alert schema");
        store.insert_bar(&bar(1.05)).expect("insert raw bar");

        {
            let conn = store.pool.get().unwrap();
            conn.execute(
                "INSERT INTO smt_divergences
                    (id, watchlist_id, sweeper_symbol, comparison_tf, direction,
                     detection_state, payload_json, ts_created, ts_updated)
                 VALUES ('legacy-smt', 'wl', 'TVC:DXY', '30m', 'bullish',
                         'c2_confirmed', '{}', 1, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ict_structures
                    (id, symbol, tf, kind, state, payload_json, ts_created, ts_updated)
                 VALUES ('legacy-dxy-h4', 'TVC:DXY', '4h', 'fvg', 'active', '{}', 1, 1),
                        ('unrelated-btc-h4', 'COINBASE:BTCUSD', '4h', 'fvg', 'active', '{}', 1, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO detector_config (detector, key, value)
                 VALUES ('smt', 'derived_rule_version', 'legacy')
                 ON CONFLICT(detector, key) DO UPDATE SET value = 'legacy'",
                [],
            )
            .unwrap();
        }

        assert_eq!(
            store.reset_derived_smt_pipeline_for_current_rule().unwrap(),
            2
        );
        assert_eq!(
            store.count("OANDA:EURUSD", Timeframe::M5.tag()).unwrap(),
            1,
            "raw market bars must survive the derived-data rebuild"
        );
        let conn = store.pool.get().unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM smt_divergences", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM ict_structures WHERE id = 'legacy-dxy-h4'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "stale core-FX HTF structures must be rebuilt on the canonical grid"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM ict_structures WHERE id = 'unrelated-btc-h4'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1,
            "unrelated symbols must survive the SMT migration"
        );
        assert_eq!(
            conn.query_row(
                "SELECT value FROM detector_config
                 WHERE detector = 'smt' AND key = 'derived_rule_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            CURRENT_PIPELINE_VERSION
        );
        drop(conn);
        assert_eq!(
            store.reset_derived_smt_pipeline_for_current_rule().unwrap(),
            0
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn source_smt_invalidation_repairs_nonterminal_candidates() {
        use crate::candidate::{DecisionStatus, ExpiryReason, SetupStatus, SetupType};
        use crate::detector::types::{CandleRef, Direction};

        let path = temp_db_path("smt-candidate-invalidation");
        let store = SqliteStore::open(&path).expect("open test db");
        store
            .ensure_detector_config_schema()
            .expect("detector config schema");
        store.ensure_candidate_schema().expect("candidate schema");
        let candle = CandleRef {
            ts: 1_000,
            open: 1.0,
            high: 1.1,
            low: 0.9,
            close: 1.0,
        };
        let candidate = CandidateSetup {
            id: "candidate-1".into(),
            rule_version: "m6a.v1".into(),
            watchlist_id: "wl".into(),
            smt_id: "smt-1".into(),
            setup_type: SetupType::Smt,
            symbol_set: vec!["TVC:DXY".into(), "OANDA:EURUSD".into()],
            sweeper_symbol: "TVC:DXY".into(),
            trade_symbols: vec!["OANDA:EURUSD".into()],
            candidate_direction: Direction::Bullish,
            context_timeframe: Timeframe::H1,
            comparison_timeframe: Timeframe::M30,
            validation_timeframe: Timeframe::M5,
            context_pda_id: Some("pda-1".into()),
            observation_window: (0, 2_000),
            c1_candle: candle.clone(),
            smt_k_candle: candle.clone(),
            c2_candle: candle,
            c2_case: 1,
            c3_candle: None,
            c2_cisd_event_ids: vec![],
            c3_cisd_event_ids: vec![],
            validation_kind: None,
            validation_symbol: None,
            validation_ts: None,
            validation_direction: None,
            validations: vec![],
            invalidated_symbols: vec![],
            symbol_invalidation_reasons: Default::default(),
            setup_status: SetupStatus::ReCheck,
            decision_status: DecisionStatus::New,
            deterministic_score: 0.4,
            created_at: 1_000,
            validated_at: None,
            expired_at: Some(2_000),
            invalidated_at: None,
            expiry_reason: Some(ExpiryReason::Ttl),
            strength: vec![],
            expiry_at: Some(2_000),
            smt_rule_version: CURRENT_RULE_VERSION.into(),
        };
        store.upsert_candidate(&candidate, 2_000).unwrap();

        assert_eq!(
            store
                .mark_candidates_smt_invalidated("smt-1", 3_000)
                .unwrap(),
            1
        );
        let repaired = store.list_all_candidates("wl").unwrap();
        assert_eq!(repaired[0].setup_status, SetupStatus::Invalidated);
        assert_eq!(
            repaired[0].expiry_reason,
            Some(ExpiryReason::SmtInvalidated)
        );
        assert_eq!(repaired[0].invalidated_at, Some(3_000));
        assert_eq!(
            repaired[0].symbol_invalidation_reasons.get("OANDA:EURUSD"),
            Some(&ExpiryReason::SmtInvalidated)
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalidated_smt_remains_in_audit_history_with_terminal_state() {
        use crate::detector::types::{
            CandleRef, Correlation, Direction, LiquidityRef, LiquidityRefStatus, LiquiditySide,
            ReferenceScope, SmtDetectionState, SmtDivergence, StrengthLabel, SymbolChain,
        };

        let path = temp_db_path("smt-audit-history");
        let store = SqliteStore::open(&path).expect("open test db");
        store
            .ensure_detector_config_schema()
            .expect("detector config schema");
        store.ensure_ict_schema().expect("ict schema");
        store.ensure_smt_schema().expect("smt schema");
        let candle = CandleRef {
            ts: 1_000,
            open: 1.0,
            high: 1.1,
            low: 0.9,
            close: 1.0,
        };
        let divergence = SmtDivergence {
            id: "smt-audit-1".into(),
            watchlist_id: "wl".into(),
            rule_version: CURRENT_RULE_VERSION.into(),
            symbol_set: vec!["TVC:DXY".into(), "OANDA:EURUSD".into()],
            relationship: Correlation::Negative,
            context_timeframe: Timeframe::H1,
            comparison_timeframe: Timeframe::M30,
            observation_window: (0, 2_000),
            htf_confirmed: true,
            reference_scope: ReferenceScope::DistantLeftSide,
            liquidity_refs: vec![LiquidityRef {
                symbol: "TVC:DXY".into(),
                ref_price: 1.0,
                ref_ts: 0,
                side: LiquiditySide::SellSide,
                status: LiquidityRefStatus::Swept,
                tf: Timeframe::H1,
                mtf_ref_candle: None,
                mtf_sweep_candle: None,
            }],
            confluence_refs: Vec::new(),
            candidate_direction: Direction::Bullish,
            sweeper_symbol: "TVC:DXY".into(),
            trade_symbols: vec!["OANDA:EURUSD".into()],
            strength: vec![StrengthLabel {
                symbol: "OANDA:EURUSD".into(),
                label: "strong".into(),
            }],
            chains: vec![SymbolChain {
                symbol: "TVC:DXY".into(),
                c1_candle: candle.clone(),
                smt_k_candle: candle.clone(),
                c2_candle: Some(candle.clone()),
                c2_case: Some(1),
                c3_candle: None,
                detection_state: SmtDetectionState::C2Confirmed,
            }],
            invalidation_reasons: Vec::new(),
            invalidation_ts: None,
            htf_pda_ref: None,
            mtf_ref_candle: None,
        };
        store.upsert_smt(&divergence, 2_000).unwrap();
        let mut canonical = divergence.clone();
        canonical.liquidity_refs[0].ref_price = 1.05;
        canonical.liquidity_refs[0].ref_ts = 500;
        canonical.trade_symbols.clear();
        canonical.strength[0].label = "weak".into();
        store
            .mark_smt_invalidated_with_snapshot(
                &canonical,
                3_000,
                &[SmtInvalidationReason::AllCountersSwept],
            )
            .unwrap();

        assert!(store.list_active_smt("wl").unwrap().is_empty());
        let audit = store.list_all_smt("wl").unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(
            audit[0].chains[0].detection_state,
            SmtDetectionState::Invalidated
        );
        assert_eq!(
            audit[0].invalidation_reasons,
            vec![SmtInvalidationReason::AllCountersSwept]
        );
        assert_eq!(audit[0].liquidity_refs[0].ref_ts, 500);
        assert_eq!(audit[0].liquidity_refs[0].ref_price, 1.05);
        assert!(audit[0].trade_symbols.is_empty());
        assert_eq!(audit[0].strength[0].label, "weak");

        let _ = std::fs::remove_file(path);
    }
}
