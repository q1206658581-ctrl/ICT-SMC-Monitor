mod model;
mod report;
mod simulator;
mod source;
#[cfg(test)]
mod tests;

use anyhow::{bail, Context, Result};
use model::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

pub fn cli(args: Vec<String>) -> Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("backtest-eval [--source PATH] [--eval-db PATH] [--reports DIR] [--cohort NAME] [--mode targets|fixed-r|user-v1] [--entry-bars 288] [--holding-bars N] [--ttl-mtf-bars 5]\nProduction is read-only. Completed cohorts are immutable; choose a new cohort to include new data. No network or notifications.");
        return Ok(());
    }
    let home = PathBuf::from(std::env::var("HOME").context("HOME is unset")?).join(".ict-monitor");
    let mut source = home.join("ict.db");
    let mut eval = home.join("backtest_eval.db");
    let mut reports = home.join("backtest_reports");
    let mut p = Parameters {
        version: VERSION.into(),
        source: String::new(),
        cohort: "initial".into(),
        entry_bars: 288,
        holding_bars: None,
        ttl_mtf_bars: 5,
        mode: ExitMode::Targets,
        guardrail_version: ict_monitor::llm::M7D_STRATEGY_VERSION.into(),
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .with_context(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--source" => source = PathBuf::from(value),
            "--eval-db" => eval = PathBuf::from(value),
            "--reports" => reports = PathBuf::from(value),
            "--cohort" => p.cohort = value.clone(),
            "--mode" => {
                p.mode = match value.as_str() {
                    "targets" => ExitMode::Targets,
                    "fixed-r" => ExitMode::FixedR,
                    "user-v1" => ExitMode::UserV1,
                    _ => bail!("mode must be targets, fixed-r or user-v1"),
                }
            }
            "--entry-bars" => p.entry_bars = value.parse()?,
            "--holding-bars" => p.holding_bars = Some(value.parse()?),
            "--ttl-mtf-bars" => p.ttl_mtf_bars = value.parse()?,
            _ => bail!("unknown option {flag}"),
        }
    }
    if p.cohort.trim().is_empty()
        || p.entry_bars == 0
        || p.entry_bars > 100_000
        || p.holding_bars.is_some_and(|n| n == 0 || n > 100_000)
        || p.ttl_mtf_bars == 0
        || p.ttl_mtf_bars > 100
    {
        bail!("invalid cohort/window parameters")
    }
    p.source = source
        .canonicalize()
        .context("source database must exist")?
        .to_string_lossy()
        .into_owned();
    validate_paths(Path::new(&p.source), &eval, &reports)?;
    let result = run(p, &eval)?;
    std::fs::create_dir_all(&reports)?;
    let md = reports.join(format!("{}.md", result.run_id));
    let csv = reports.join(format!("{}.csv", result.run_id));
    for path in [&md, &csv] {
        if path.is_symlink() {
            bail!("refusing symlink report output: {}", path.display())
        }
    }
    for path in [&md, &csv] {
        validate_paths(
            Path::new(&result.parameters.source),
            path,
            &reports.join("reserved"),
        )?;
        if path.exists() && path.canonicalize()? == eval.canonicalize()? {
            bail!("report aliases eval database")
        }
    }
    write_report(&md, report::markdown(&result).as_bytes())?;
    write_report(&csv, report::csv(&result).as_bytes())?;
    if result.parameters.mode == ExitMode::UserV1 {
        println!("{}", report::user_summary(&result));
        println!("Report: {}\nCSV: {}", md.display(), csv.display());
        return Ok(());
    }
    if result.parameters.mode == ExitMode::FixedR {
        println!("固定 R 模式：C2 入场时机的方向性优势，非生产策略 RR。");
        for r in 1..=3 {
            let projection = report::project_r(&result, r);
            let rows: Vec<_> = projection.trades.iter().collect();
            let s = report::stats(&rows);
            println!(
                "+{r}R: resolved={}, wins={}, mean RR={:?}",
                s.resolved, s.wins, s.mean
            );
        }
        println!("Report: {}\nCSV: {}", md.display(), csv.display());
        return Ok(());
    }
    let rows: Vec<_> = result.trades.iter().collect();
    let stats = report::stats(&rows);
    println!("M8 Lite {}: C2={}, resolved win/loss={}, wins={}, mean realized RR={:?}\nReport: {}\nCSV: {}",result.run_id,result.c2_count,stats.resolved,stats.wins,stats.mean,md.display(),csv.display());
    let mut outcomes = std::collections::BTreeMap::new();
    let mut issues = std::collections::BTreeMap::new();
    for trade in &result.trades {
        *outcomes
            .entry(trade.simulation.outcome.tag())
            .or_insert(0usize) += 1;
        for issue in &trade.data_issues {
            *issues.entry(issue.as_str()).or_insert(0usize) += 1;
        }
    }
    println!("Outcomes: {outcomes:?}\nData issues (overlapping): {issues:?}");
    if stats.resolved < 10 {
        println!("已完成交易不足 10，不能判断告警是否有优势；N/A 不等于零胜率。");
    }
    Ok(())
}

fn validate_paths(source: &Path, eval: &Path, reports: &Path) -> Result<()> {
    let absolute = |p: &Path| -> Result<PathBuf> {
        if p.exists() {
            return Ok(p.canonicalize()?);
        }
        let mut ancestor = p;
        let mut suffix = Vec::new();
        while !ancestor.exists() {
            suffix.push(
                ancestor
                    .file_name()
                    .context("invalid output path")?
                    .to_owned(),
            );
            ancestor = ancestor
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
        }
        let mut resolved = ancestor.canonicalize()?;
        for item in suffix.iter().rev() {
            resolved.push(item)
        }
        Ok(resolved)
    };
    let eval_abs = absolute(eval)?;
    let reports_abs = absolute(reports)?;
    if eval_abs == source
        || reports_abs == source
        || eval_abs == reports_abs
        || eval_abs.starts_with(&reports_abs)
    {
        bail!("source/eval/report paths must be separate")
    }
    // Also reject SQLite companion paths and hard links to the production DB.
    for suffix in ["-wal", "-shm", "-journal"] {
        if eval_abs.as_os_str() == format!("{}{suffix}", source.display()).as_str() {
            bail!("eval path aliases source SQLite companion")
        }
    }
    #[cfg(unix)]
    if eval.exists() {
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(source)?;
        let b = std::fs::metadata(eval)?;
        if a.dev() == b.dev() && a.ino() == b.ino() {
            bail!("eval path hard-links source")
        }
    }
    Ok(())
}

fn run(p: Parameters, eval_path: &Path) -> Result<Evaluation> {
    let json = serde_json::to_string(&p)?;
    let id = blake3::hash(json.as_bytes()).to_hex().to_string();
    if let Some(parent) = eval_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?
        }
    }
    let mut db = Connection::open(eval_path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    let foreign_tables: usize=db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT IN ('eval_runs','eval_trades') AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;
    if foreign_tables > 0 {
        bail!("eval database contains non-evaluation tables")
    }
    db.execute_batch("CREATE TABLE IF NOT EXISTS eval_runs(run_id TEXT PRIMARY KEY,parameters_json TEXT NOT NULL,result_json TEXT NOT NULL); CREATE TABLE IF NOT EXISTS eval_trades(run_id TEXT NOT NULL,alert_id TEXT NOT NULL,payload_json TEXT NOT NULL,PRIMARY KEY(run_id,alert_id));")?;
    if let Some(json) = db
        .query_row(
            "SELECT result_json FROM eval_runs WHERE run_id=?1",
            [&id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(serde_json::from_str(&json)?);
    }
    let result = source::evaluate(p, id.clone())?;
    let transaction = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Concurrent same-cohort runs must return the first committed snapshot.
    if let Some(json) = transaction
        .query_row(
            "SELECT result_json FROM eval_runs WHERE run_id=?1",
            [&id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(serde_json::from_str(&json)?);
    }
    let result_json = serde_json::to_string(&result)?;
    transaction.execute(
        "INSERT INTO eval_runs VALUES(?1,?2,?3)",
        params![id, json, result_json],
    )?;
    for t in &result.trades {
        transaction.execute(
            "INSERT INTO eval_trades VALUES(?1,?2,?3)",
            params![id, t.alert_id, serde_json::to_string(t)?],
        )?;
    }
    transaction.commit()?;
    // First and cached runs both render the exact persisted representation.
    Ok(serde_json::from_str(&result_json)?)
}

fn write_report(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let operation = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if operation.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    operation
}
