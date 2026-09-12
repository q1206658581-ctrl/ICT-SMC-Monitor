use super::model::*;
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct Stats {
    pub n: usize,
    pub resolved: usize,
    pub wins: usize,
    pub mean: Option<f64>,
    pub median: Option<f64>,
    pub profit_factor: Option<f64>,
    pub mfe: Option<f64>,
    pub mae: Option<f64>,
    pub floating: Option<f64>,
    pub excursions_n: usize,
    pub expired_n: usize,
}
fn avg(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}
pub fn stats(rows: &[&Trade]) -> Stats {
    let mut rr: Vec<_> = rows
        .iter()
        .filter_map(|t| t.simulation.realized_rr)
        .collect();
    rr.sort_by(f64::total_cmp);
    let gains: f64 = rr.iter().filter(|r| **r > 0.0).sum();
    let losses: f64 = rr.iter().filter(|r| **r < 0.0).map(|r| -r).sum();
    let mfe: Vec<_> = rows
        .iter()
        .filter(|t| {
            matches!(
                t.simulation.outcome,
                Outcome::Win | Outcome::Loss | Outcome::Expired
            )
        })
        .filter_map(|t| t.simulation.mfe_rr)
        .collect();
    let mae: Vec<_> = rows
        .iter()
        .filter(|t| {
            matches!(
                t.simulation.outcome,
                Outcome::Win | Outcome::Loss | Outcome::Expired
            )
        })
        .filter_map(|t| t.simulation.mae_rr)
        .collect();
    let floating: Vec<_> = rows
        .iter()
        .filter_map(|t| t.simulation.floating_rr)
        .collect();
    Stats {
        n: rows.len(),
        resolved: rr.len(),
        wins: rows
            .iter()
            .filter(|t| t.simulation.outcome == Outcome::Win)
            .count(),
        mean: avg(&rr),
        median: (!rr.is_empty()).then(|| (rr[(rr.len() - 1) / 2] + rr[rr.len() / 2]) / 2.0),
        profit_factor: (losses > 0.0).then_some(gains / losses),
        mfe: avg(&mfe),
        mae: avg(&mae),
        floating: avg(&floating),
        excursions_n: mfe.len(),
        expired_n: floating.len(),
    }
}
fn number(v: Option<f64>) -> String {
    v.map(|n| format!("{n:.4}")).unwrap_or_else(|| "N/A".into())
}
fn safe(s: &str) -> String {
    s.replace('|', "/").replace(['\n', '\r'], " ")
}
fn standard_markdown(e: &Evaluation) -> String {
    let all: Vec<_> = e.trades.iter().collect();
    let summary = stats(&all);
    let mut out=format!("# M8 Lite 告警结局评估\n\nRun: `{}`\n\n参数：`{}`\n\n源数据各品种截止的最大值（UTC，非统一模拟截止）：{}；C2 {} 条，其他告警 {} 条不属于本次评估。\n\n",e.run_id,serde_json::to_string(&e.parameters).unwrap(),timestamp(e.source_cutoff_ts),e.c2_count,e.other_alert_count);
    out.push_str("各品种独立收盘截止（UTC）：每个品种仅使用开盘时间早于自身截止的 M1；最新一根视为可能未收盘。自身序列耗尽记 insufficient_data，序列内部缺分钟记 data_gap。\n\n");
    for (symbol, cutoff) in &e.symbol_cutoffs {
        out.push_str(&format!("- {}：{}\n", symbol, timestamp(*cutoff)));
    }
    out.push('\n');

    let min = e.trades.iter().map(|t| t.as_of_ts).min().unwrap_or(0);
    let max = e.trades.iter().map(|t| t.as_of_ts).max().unwrap_or(0);
    out += &format!("告警窗口：{} 至 {}。\n\n", timestamp(min), timestamp(max));
    if summary.resolved < 10 {
        out+="**已完成交易样本不足 10，不能判断这些告警是否有优势。** 数据不足、无目标或缺失证据不代表策略输赢。\n\n";
    } else {
        out+="**以下仅描述已记录告警在指定假设下的样本表现，不据此认定稳定优势。** 尚未覆盖成本与多个市场状态；本报告没有改动策略或阈值。\n\n";
    }
    let missing_targets = e
        .trades
        .iter()
        .filter(|t| t.data_issues.iter().any(|s| s == "missing_target"))
        .count();
    if missing_targets > 0 {
        let sources = if e.parameters.guardrail_version == "m7d.decision_rules.v2" {
            "SMT参照及已确认、可证明未扫的L1流动性"
        } else {
            "SMT liquidity_refs"
        };
        out+=&format!("**{missing_targets}/{} 条告警没有 target1。** 本次共享护栏来源为{sources}；没有目标就无法按本规则评估收益，不等于策略没有优势。\n\n",e.c2_count);
    }
    out += "## 总结局分布与数据体检\n\n| 结局 | 样本量 |\n| --- | ---: |\n";
    for outcome in [
        Outcome::Win,
        Outcome::Loss,
        Outcome::Skip,
        Outcome::Ambiguous,
        Outcome::NoFill,
        Outcome::Expired,
        Outcome::DataGap,
        Outcome::InsufficientData,
        Outcome::Excluded,
    ] {
        out += &format!(
            "| {} | {} |\n",
            outcome.tag(),
            e.trades
                .iter()
                .filter(|t| t.simulation.outcome == outcome)
                .count()
        );
    }
    out+="\nambiguous 属于未入场 skip 口径，但上表独立展示、不重复计数。data_gap / insufficient_data / excluded 均不进入胜率、realized RR 或 profit factor。\n\n";
    out += "数据体检按问题分别计数，同一告警可能有多个问题，以下计数不可相加当作告警总数。\n\n";
    let mut reasons = BTreeMap::<String, usize>::new();
    for t in &e.trades {
        for issue in &t.data_issues {
            *reasons.entry(issue.clone()).or_default() += 1;
        }
    }
    let mut gap_categories = BTreeMap::<String, usize>::new();
    for t in &e.trades {
        if let Some(g) = &t.anchor_gap {
            *gap_categories.entry(g.category.clone()).or_default() += 1;
        }
    }
    if !gap_categories.is_empty() {
        out += "### 锚点 M1 缺失分类\n\n| 类别 | 告警数 |\n| --- | ---: |\n";
        for (category, n) in gap_categories {
            out += &format!("| {category} | {n} |\n");
        }
        out+="\nCSV anchor_gap_json 保存首末覆盖、前后相邻分钟及比较周期收盘是否存在。高周期 OHLC 无法唯一还原分钟路径；本次不插值、不回填生产。外部历史源能否补齐需以其实际返回为准，不能仅凭存在高周期数据认定可恢复。\n\n";
    }
    out += "| 不参与收益统计的原因 | 样本量 |\n| --- | ---: |\n";
    for (reason, n) in reasons {
        out += &format!("| {} | {n} |\n", safe(&reason));
    }
    out+="\n## 总体与分桶统计\n\n胜率 = win/(win+loss)；RR 均值、中位数及 profit factor 仅使用 win/loss；PF = 正 RR 总和 / 负 RR 绝对值总和，无亏损时记 N/A。expired 只报告期末浮动 RR，不假设强制平仓。MFE/MAE 单位为初始风险 R，统计完整结局 win/loss/expired，分别给出样本量。\n\n";
    out+="| 分桶 | N | win/loss N | 胜率 | RR均值 | RR中位数 | PF | MFE/MAE N | avg MFE | avg MAE | expired N | avg浮动RR | 提示 |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n";
    let mut buckets = BTreeMap::<String, Vec<&Trade>>::new();
    buckets.insert("全部".into(), all);
    for t in &e.trades {
        let confidence = match t.confidence {
            Some(0..=49) => "00-49",
            Some(50..=64) => "50-64",
            Some(65..=79) => "65-79",
            Some(_) => "80-100",
            None => "unknown",
        };
        for (dimension, key) in [
            ("方向", t.direction.as_str()),
            ("品种", t.symbol.as_str()),
            ("session", t.session.as_str()),
            ("置信度", confidence),
            ("质量", t.quality.as_str()),
            ("每周时段", t.weekly_slot.as_str()),
        ] {
            buckets
                .entry(format!("{dimension}={key}"))
                .or_default()
                .push(t);
        }
        buckets
            .entry(format!(
                "交叉={}/{}/{}/{}/{}/{}",
                t.direction, t.symbol, t.session, confidence, t.quality, t.weekly_slot
            ))
            .or_default()
            .push(t);
    }
    for (label, rows) in buckets {
        let s = stats(&rows);
        let win_rate = (s.resolved > 0).then(|| s.wins as f64 / s.resolved as f64);
        out += &format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            safe(&label),
            s.n,
            s.resolved,
            number(win_rate),
            number(s.mean),
            number(s.median),
            number(s.profit_factor),
            s.excursions_n,
            number(s.mfe),
            number(s.mae),
            s.expired_n,
            number(s.floating),
            if s.n < 10 || s.resolved < 10 {
                "小样本，不可下结论"
            } else {
                "描述性统计"
            }
        );
    }
    out+="\n## 模拟假设与限制\n\n- 生产只读；首轮成功结果冻结在独立 eval 库。相同参数/cohort 重跑复用冻结结果；新数据使用新 `--cohort`，不会悄悄改写旧报告。最新一根 M1 视为可能未收盘，不进入模拟。\n- 告警行是决策单元；跨品种/同 SMT 告警可能相关，不能当独立随机样本。历史补录告警也纳入，本报告评估市场锚点的理论交易，不等于当时用户实际收到通知后的可执行收益。\n- 使用告警 created_at 市场时间；同一 Context Packer 的显式 as-of 路径裁剪未来 K/C3/结构。护栏复算自现存历史证据，证据后来被覆盖或修订的偏差无法凭空消除。\n- 入场窗口 W 默认 288 根 M1。H 默认 5×比较周期分钟数，从成交后计；沿用 TTL 长度，不使用后来出现的 C3/expiry_at 推断过去的持有期。\n- 锚点价格取恰在锚点收盘的 M1 close；在区间内立即成交。后续触区间用不利边界，多头 high / 空头 low。跳空未覆盖入场区不臆造成交；触止损仍按规则 -1R，不建模滑点。\n- 同根入场与止损：ambiguous，未入场。同根目标与止损：loss。盘中入场根不授予目标命中；从下一根完整 M1 开始持有计数，避免假设该根高低点发生在入场后。\n- time_to_entry 从告警算起；time_to_target/stop 从成交算起，单位毫秒，触价时间记录该 M1 收盘边界，精度为一分钟。\n- MFE/MAE 为保守可确认的路径幅度：盘中入场根忽略，终止根只纳入退出价，不将退出后的极值算进持仓；不是 tick 精度的精确极值。\n- 缺失分钟立即标 data_gap；包括无法区分的休市间隔，不跨缺口猜测结局。数据尾部未覆盖至结局为 insufficient_data，不伪装为 expired/no_fill。\n- Session 使用纽约本地时间（自动 DST）：Asia 20–24；London 02–05、10–12；NY 07–10；其余 Off-hours，沿用项目 session 窗口。每周时段为纽约星期×4小时。\n- 不含点差、佣金、滑点、资金管理；约三个月夏季市场窗口存在选择/生存偏差。小样本与相关样本不能支持精细归因或外推。\n";
    out
}
fn timestamp(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| ts.to_string())
}
fn cell(s: impl ToString) -> String {
    format!("\"{}\"", s.to_string().replace('"', "\"\""))
}
fn standard_csv(e: &Evaluation, multiple: Option<f64>, buffer: Option<u8>) -> String {
    let mut out="exit_mode,r_multiple,run_id,alert_id,candidate_id,smt_id,watchlist_id,symbol,as_of_ts,direction,confidence,quality,session,weekly_slot,holding_bars,outcome,reason,entry,entry_ts,exit_ts,realized_rr,floating_rr,mfe_rr,mae_rr,time_to_entry_ms,time_to_target_ms,time_to_stop_ms,production_theoretical_rr,price_guardrails_json,evidence_hash,data_issues_json,available_future_m1,anchor_gap_json\n".to_owned();
    if buffer.is_some() {
        out =
            out.trim_end().to_owned() + ",buffer_points,user_stop,user_target,target1_in_user_R\n";
    }
    for t in &e.trades {
        let s = &t.simulation;
        let opt_i = |x: Option<i64>| x.map(|v| v.to_string()).unwrap_or_default();
        let opt_f = |x: Option<f64>| x.map(|v| v.to_string()).unwrap_or_default();
        let mut fields = vec![
            if e.parameters.mode == ExitMode::UserV1 && buffer.is_none() {
                "fixed_r_anchor".into()
            } else if e.parameters.mode == ExitMode::UserV1 {
                "user_v1".into()
            } else if e.parameters.mode == ExitMode::FixedR {
                "fixed_r".into()
            } else {
                "targets".into()
            },
            multiple.map(|r| r.to_string()).unwrap_or_default(),
            e.run_id.clone(),
            t.alert_id.clone(),
            t.candidate_id.clone(),
            t.smt_id.clone(),
            t.watchlist_id.clone(),
            t.symbol.clone(),
            t.as_of_ts.to_string(),
            t.direction.clone(),
            t.confidence.map(|x| x.to_string()).unwrap_or_default(),
            t.quality.clone(),
            t.session.clone(),
            t.weekly_slot.clone(),
            t.holding_bars.to_string(),
            s.outcome.tag().into(),
            s.reason.clone(),
            opt_f(s.entry),
            opt_i(s.entry_ts),
            opt_i(s.exit_ts),
            opt_f(s.realized_rr),
            opt_f(s.floating_rr),
            opt_f(s.mfe_rr),
            opt_f(s.mae_rr),
            opt_i(s.time_to_entry_ms),
            opt_i(s.time_to_target_ms),
            opt_i(s.time_to_stop_ms),
            opt_f(t.guardrails.as_ref().and_then(|g| g.risk_reward)),
            serde_json::to_string(&t.guardrails.as_ref().map(|g| {
                serde_json::json!({
                    "rule_version": g.rule_version,
                    "entry_zone": g.entry_zone,
                    "invalidation_price": g.invalidation_price,
                    "targets": g.targets,
                    "risk_reward": g.risk_reward,
                })
            }))
            .unwrap(),
            t.evidence_hash.clone().unwrap_or_default(),
            serde_json::to_string(&t.data_issues).unwrap(),
            t.available_future_m1.to_string(),
            serde_json::to_string(&t.anchor_gap).unwrap(),
        ];
        if let Some(buffer) = buffer {
            let u = t
                .user_v1
                .iter()
                .find(|u| u.buffer_points == buffer && Some(u.multiple) == multiple)
                .unwrap();
            fields.extend([
                buffer.to_string(),
                opt_f(u.stop),
                opt_f(u.target),
                opt_f(u.target1_in_user_r),
            ]);
        }
        out += &fields.into_iter().map(cell).collect::<Vec<_>>().join(",");
        out.push('\n');
    }
    out
}

pub fn project_r(e: &Evaluation, r: u8) -> Evaluation {
    let mut projected = e.clone();
    if e.parameters.mode != ExitMode::UserV1 {
        projected.parameters.mode = ExitMode::FixedR;
    }
    for trade in &mut projected.trades {
        trade.simulation = trade
            .fixed_r
            .iter()
            .find(|x| x.multiple == r)
            .expect("complete fixed-R triplet")
            .simulation
            .clone();
    }
    projected
}

fn quantile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let index = (v.len() - 1) as f64 * p;
    let low = index.floor() as usize;
    let high = index.ceil() as usize;
    Some(v[low] + (v[high] - v[low]) * (index - low as f64))
}

fn r_distribution(e: &Evaluation) -> String {
    let mut groups = BTreeMap::<String, Vec<&Trade>>::new();
    groups.insert("全部".into(), e.trades.iter().collect());
    for t in &e.trades {
        let confidence = match t.confidence {
            Some(0..=49) => "00-49",
            Some(50..=64) => "50-64",
            Some(65..=79) => "65-79",
            Some(_) => "80-100",
            None => "unknown",
        };
        for (dimension, key) in [
            ("方向", t.direction.as_str()),
            ("品种", t.symbol.as_str()),
            ("session", t.session.as_str()),
            ("置信度", confidence),
        ] {
            groups
                .entry(format!("{dimension}={key}"))
                .or_default()
                .push(t);
        }
    }
    let mut out="| 分桶 | 告警 N | 完整持仓 N | 先触目标率 | win/loss胜率 | 结算期望 R | 含到期期望 R | MFE P25/P50/P75/P90 | MAE P25/P50/P75/P90 | 提示 |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |\n".to_owned();
    for (label, rows) in groups {
        let stats = stats(&rows);
        let valid: Vec<_> = rows
            .iter()
            .filter(|t| {
                matches!(
                    t.simulation.outcome,
                    Outcome::Win | Outcome::Loss | Outcome::Expired
                )
            })
            .collect();
        let payoff: Vec<_> = valid
            .iter()
            .filter_map(|t| t.simulation.realized_rr.or(t.simulation.floating_rr))
            .collect();
        let mfe: Vec<_> = valid.iter().filter_map(|t| t.simulation.mfe_rr).collect();
        let mae: Vec<_> = valid.iter().filter_map(|t| t.simulation.mae_rr).collect();
        let distribution = |v: &[f64]| {
            [0.25, 0.5, 0.75, 0.9]
                .map(|p| number(quantile(v, p)))
                .join(" / ")
        };
        out += &format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            safe(&label),
            rows.len(),
            valid.len(),
            number((!valid.is_empty()).then(|| stats.wins as f64 / valid.len() as f64)),
            number((stats.resolved > 0).then(|| stats.wins as f64 / stats.resolved as f64)),
            number(stats.mean),
            number(avg(&payoff)),
            distribution(&mfe),
            distribution(&mae),
            if valid.len() < 10 || stats.resolved < 10 {
                "小样本，不可下结论"
            } else {
                "描述性统计"
            }
        );
    }
    out
}

pub fn markdown(e: &Evaluation) -> String {
    if e.parameters.mode == ExitMode::UserV1 {
        let mut out = user_summary(e);
        for buffer in USER_BUFFERS {
            for multiple in USER_MULTIPLES {
                let projected = project_user(e, buffer, multiple);
                out += &format!("\n## user-v1 buffer={buffer} 点 / +{multiple}R\n\n");
                out += &r_distribution(&projected);
                let detail = standard_markdown(&projected);
                // The old assumptions describe conditional zone entries. Use
                // this mode's explicit assumptions instead of contradictory text.
                out += detail.split("## 模拟假设与限制").next().unwrap();
            }
        }
        out += "\n## 同快照原结构止损对照（统一锚点收盘立即入场）\n\n";
        for r in 1..=3 {
            let projected = project_r(e, r);
            out += &format!("\n### 原结构止损 +{r}R\n\n");
            out += &r_distribution(&projected);
            out += standard_markdown(&projected)
                .split("## 模拟假设与限制")
                .next()
                .unwrap();
        }
        return out;
    }
    if e.parameters.mode == ExitMode::Targets {
        return standard_markdown(e);
    }
    let mut out="# 固定 R 出场评估：C2 入场时机的方向性优势\n\n本模式不使用生产目标，不代表生产策略 RR。+1R/+2R/+3R 是同一告警、同一成交的三个独立假设，不是三次独立交易或分批止盈；不可合并样本量。真实目标就绪后以真目标模式为准。\n\n1R = 实际成交价与失效位距离。先触目标率 = win/(win+loss+expired)，分母为完整持仓样本；win/loss 胜率另列。结算期望 = win/loss 的平均 realized RR；含到期期望 = (win/loss 的 RR + expired 的期末浮动 RR)/完整持仓 N（不是已实现收益）。缺口、尾部不足和未成交不进入这些分母。MFE/MAE 分位数采用线性插值，均为退出前保守可确认路径值。\n\n置信度/质量沿用参数中所列共享护栏版本；固定 R 不因虚拟目标而提升生产置信度。\n\n".to_owned();
    for r in 1..=3 {
        let projected = project_r(e, r);
        out += &format!("## +{r}R 独立出场\n\n");
        out += &r_distribution(&projected);
        out.push_str("\n\n");
        out += &standard_markdown(&projected);
    }
    out
}

pub fn csv(e: &Evaluation) -> String {
    if e.parameters.mode == ExitMode::UserV1 {
        let mut out = String::new();
        for buffer in USER_BUFFERS {
            for multiple in USER_MULTIPLES {
                let text = standard_csv(
                    &project_user(e, buffer, multiple),
                    Some(multiple),
                    Some(buffer),
                );
                out += if out.is_empty() {
                    &text
                } else {
                    text.split_once('\n').unwrap().1
                };
            }
        }
        // Three control groups use the identical source snapshot; pad only
        // user-specific fields so the CSV remains rectangular.
        for r in 1..=3 {
            let text = standard_csv(&project_r(e, r), Some(r as f64), None);
            for line in text.lines().skip(1) {
                out += line;
                out += ",,,,\n";
            }
        }
        return out;
    }
    if e.parameters.mode == ExitMode::Targets {
        return standard_csv(e, None, None);
    }
    let mut out = String::new();
    for r in 1..=3 {
        let text = standard_csv(&project_r(e, r), Some(r as f64), None);
        out += if r == 1 {
            &text
        } else {
            text.split_once('\n').unwrap().1
        };
    }
    out
}

pub fn project_user(e: &Evaluation, buffer: u8, multiple: f64) -> Evaluation {
    let mut projected = e.clone();
    for t in &mut projected.trades {
        t.simulation = t
            .user_v1
            .iter()
            .find(|u| u.buffer_points == buffer && u.multiple == multiple)
            .expect("complete user-v1 grid")
            .simulation
            .clone();
        if matches!(
            t.simulation.outcome,
            Outcome::Excluded | Outcome::InsufficientData | Outcome::DataGap
        ) && !t.data_issues.contains(&t.simulation.reason)
        {
            t.data_issues.push(t.simulation.reason.clone());
        }
    }
    projected
}

pub fn user_summary(e: &Evaluation) -> String {
    let mut out = format!(
        "# user-v1 用户策略与原结构止损：统一即时入场对照\n\nRun: `{}`；evaluator {}；cohort `{}`。\n\n",
        e.run_id, e.parameters.version, e.parameters.cohort
    );
    out += "user-v1 无条件按锚点已收盘 M1 close 立即成交，锚点根不计持有；下一根 M1 起计 H（默认 5×比较周期）。止损为交易品种当时已确认 C2 的影线极值 ± buffer，点值复用生产 truncate_price 的 price_decimals。按 kickoff 退化条款，未加缓冲的 C2 极值已不在入场不利侧则全组排除 degenerate_stop，不靠 buffer 挽救。目标为实际入场 ± r×初始风险。目标/止损同根双触按 loss；到期浮动单列。缺口不猜测，按符号排除最新可能未收盘 M1。\n\n12 组共享同一告警，不是独立交易；原结构止损 1/2/3R 对照使用同一只读事务快照、相同锚点价格立即入场，只保留原生产失效位。两组均不再检查实体边界或等待回调；均要求当时交易品种 C2 已确认。模拟不计通知及下单延迟。独立 fixed-r/targets 命令仍保留历史口径，不等于本报告的 fixed_r_anchor 对照。置信度沿用生产护栏，不因用户目标提分。target1_in_user_R 为带方向的 (生产 target1−用户入场)/用户风险，无目标或无有效风险留空；负数表示目标已在入场后方，不取绝对值美化距离。\n\n胜率只用 Win/Loss，期望仅已结算 RR；expired 浮动分布为 P25/P50/P75/P90（线性插值）。不含点差、滑点、费用与通知延迟；历史证据修订、样本相关及单一市场窗口限制外推。只读生产，模拟字段仅在 eval 库；同 cohort 冻结，新数据须新 cohort。\n\n| 模式 | buffer 点 | R | N | W | L | expired | gap | insufficient | excluded/其他 | 结算 N | 胜率 | 结算均值 RR | expired均值 | expired P25/P50/P75/P90 | 提示 |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |\n";
    let mut groups = Vec::new();
    for buffer in USER_BUFFERS {
        for r in USER_MULTIPLES {
            groups.push(("user-v1", buffer.to_string(), r, project_user(e, buffer, r)));
        }
    }
    for r in 1..=3 {
        groups.push(("原结构止损", "—".into(), r as f64, project_r(e, r)));
    }
    for (mode, buffer, r, group) in groups {
        let rows: Vec<_> = group.trades.iter().collect();
        let st = stats(&rows);
        let count = |o| rows.iter().filter(|t| t.simulation.outcome == o).count();
        let floating: Vec<_> = rows
            .iter()
            .filter_map(|t| t.simulation.floating_rr)
            .collect();
        let w = count(Outcome::Win);
        let l = count(Outcome::Loss);
        let ex = count(Outcome::Expired);
        let gap = count(Outcome::DataGap);
        let insufficient = count(Outcome::InsufficientData);
        out += &format!("| {mode} | {buffer} | {r} | {} | {w} | {l} | {ex} | {gap} | {insufficient} | {} | {} | {} | {} | {} | {} | {} |\n",
            rows.len(), rows.len()-w-l-ex-gap-insufficient, st.resolved,
            number((st.resolved>0).then(|| st.wins as f64/st.resolved as f64)), number(st.mean), number(st.floating),
            [0.25,0.5,0.75,0.9].map(|p|number(quantile(&floating,p))).join(" / "),
            if st.resolved<10 {"小样本，不可下结论"} else if ex>0 && ex<10 {"结算为描述统计；到期样本<10，不可下结论"} else {"描述性统计"});
    }
    out += "\n## 流动性 target1 在用户风险尺度下的位置\n\n每个 buffer 每条告警只计一次，不把四个 R 组重复计数；有效 target1 N<10 时分位数不可下结论。\n\n| buffer 点 | 有效 target1 N | P25 | P50 | P75 | P90 |\n| --- | ---: | ---: | ---: | ---: | ---: |\n";
    for buffer in USER_BUFFERS {
        let v: Vec<_> = e
            .trades
            .iter()
            .filter_map(|t| {
                t.user_v1
                    .iter()
                    .find(|u| u.buffer_points == buffer && u.multiple == 1.0)
                    .and_then(|u| u.target1_in_user_r)
            })
            .collect();
        out += &format!(
            "| {buffer} | {} | {} | {} | {} | {} |\n",
            v.len(),
            number(quantile(&v, 0.25)),
            number(quantile(&v, 0.5)),
            number(quantile(&v, 0.75)),
            number(quantile(&v, 0.9))
        );
    }
    out
}
