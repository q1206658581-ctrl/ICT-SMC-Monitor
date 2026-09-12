import { useState } from 'react';
import { Parameter, Help } from './ParameterHelp';
import { INDICATOR_HELP } from './indicatorHelp';
import { invoke } from '@tauri-apps/api/core';
import { ChevronRight } from 'lucide-react';
import { useChartStore, useDetectorStore, useLayoutStore } from '../../store';
import { Button } from '../ui/button';
import { Switch } from '../ui/switch';
import type { Fvg } from '../../types/structures';

const FVG_STATE_LABELS: Record<Fvg['state'], { label: string; hint: string }> = {
  active: { label: 'Active', hint: '显示刚形成、尚未被价格触及的缺口，以虚线表示。取消勾选只隐藏该状态。' },
  mitigated_50: { label: 'Mitigated 50%', hint: '显示价格已触及缺口中线、尚未完全填补的缺口。取消勾选只隐藏该状态。' },
  filled: { label: 'Filled', hint: '显示已经被价格完全填补的历史缺口，便于复盘；填补不代表未来一定反转。此选项只影响显示。' },
  inverted_active: { label: 'Inverted Active (IFVG)', hint: '显示缺口填补后从相反一侧再次进入的反转缺口（IFVG），边框加粗。此选项只影响显示。' },
  inverted_mitigated: { label: 'Inverted Mitigated', hint: '显示反转缺口后来再次被对侧穿越、已经失效的历史状态。此选项只影响显示。' },
};

function setParam(name: string, key: string, value: unknown) {
  invoke('set_detector_param', { name, key, value })
    .then(() => useChartStore.getState().requestStructuresReload())
    .catch((e) => console.warn('set_detector_param', e));
}

function setAlertParam(key: string, value: unknown) {
  invoke('set_alert_param', { key, value })
    .catch((e) => console.warn('set_alert_param', e));
}

function Card(props: { title: string; right?: React.ReactNode; children?: React.ReactNode }) {
  const [collapsed, setCollapsed] = useState(() => {
    try { return localStorage.getItem(`indicator-collapsed:${props.title}`) === 'true'; }
    catch { return false; }
  });
  const help = INDICATOR_HELP[props.title];
  return (
    <section className="px-3 py-2 mb-2 rounded" style={{ background: 'var(--bg-2)', border: '1px solid var(--border)' }}>
      <div className="flex items-center justify-between gap-2">
        <Help text={help}>
          <button className="flex items-center gap-1 text-left text-text-1 text-sm font-medium" aria-expanded={!collapsed}
            onClick={() => {
              setCollapsed(!collapsed);
              try { localStorage.setItem(`indicator-collapsed:${props.title}`, String(!collapsed)); } catch { /* optional preference */ }
            }}>
            <ChevronRight size={13} className="shrink-0" style={{ transform: collapsed ? undefined : 'rotate(90deg)' }} />
            {props.title}
          </button>
        </Help>
        <Help text={help}><span className="inline-flex" tabIndex={0}>{props.right}</span></Help>
      </div>
      {!collapsed && <div className="flex flex-col gap-2 text-sm mt-2" style={{ color: 'var(--text-2)' }}>{props.children}</div>}
    </section>
  );
}

export function RightDrawer() {
  const layout = useLayoutStore();
  const d = useDetectorStore();
  const chart = useChartStore();

  return (
    <div
      className="h-full flex flex-col"
      style={{
        background: 'var(--bg-1)',
        borderLeft: '1px solid var(--border)',
      }}
    >
      <div
        className="flex items-center justify-between px-3 py-2"
        style={{ borderBottom: '1px solid var(--border)' }}
      >
        <span className="text-md font-medium" style={{ color: 'var(--text-1)' }}>
          指标 &amp; 参数
        </span>
        <Button
          size="icon"
          variant="ghost"
          aria-label="collapse drawer"
          onClick={() => layout.setRightCollapsed(true)}
        >
          <ChevronRight size={14} />
        </Button>
      </div>
      <div className="flex-1 overflow-y-auto p-2">
        <Card
          title="SMT 策略（C1/C2/C3）"
          right={
            <Switch
              checked={chart.smtEnabled}
              onCheckedChange={(b) => { chart.setSmtEnabled(b); }}
              ariaLabel="toggle smt master"
            />
          }
        >
          <div className="flex flex-col gap-2">
            <Parameter className="flex items-center justify-between gap-2 text-xs text-text-2" title="在图上显示策略各阶段的 K 线标记，帮助回看信号如何形成。关闭只隐藏标记，不停止策略检测或告警。">
              <span>C1/SMT K/C2/C3 链标记</span>
              <Switch
                checked={chart.smtChainEnabled}
                onCheckedChange={(b) => { chart.setSmtChainEnabled(b); }}
                ariaLabel="toggle smt chain"
              />
            </Parameter>
            <Parameter className="flex items-center justify-between gap-2 text-xs text-text-2" title="显示策略引用的较大周期价格区域，例如缺口。用于理解信号发生的位置；关闭只隐藏这层图形。">
              <span>HTF PDA 上下文 zone</span>
              <Switch
                checked={chart.smtHtfPdaEnabled}
                onCheckedChange={(b) => { chart.setSmtHtfPdaEnabled(b); }}
                ariaLabel="toggle smt htf pda"
              />
            </Parameter>
            <Parameter className="flex items-center justify-between gap-2 text-xs text-text-2" title="用白线连接发生 SMT 比较的高点或低点，方便比较组内品种的突破差异。关闭只隐藏连线。">
              <span>白线 SMT 清扫连线</span>
              <Switch
                checked={chart.smtSweepLineEnabled}
                onCheckedChange={(b) => { chart.setSmtSweepLineEnabled(b); }}
                ariaLabel="toggle smt sweep line"
              />
           </Parameter>
         </div>
       </Card>

        <Card
          title="告警（Alerts）"
          right={
            <Switch
              checked={chart.alertEnabled}
              onCheckedChange={(b) => { chart.setAlertEnabled(b); setAlertParam('enabled', b); }}
              ariaLabel="toggle alert master"
            />
          }
        >
          <div className="flex flex-col gap-2">
            <Parameter className="flex items-center justify-between gap-2 text-xs text-text-2" title="开启后，新告警可发送 macOS 桌面通知；还需要系统允许 APP 通知。关闭不影响 Inbox 中保存的告警。">
              <span>桌面通知</span>
              <Switch
                checked={chart.desktopNotifyEnabled}
                onCheckedChange={(b) => { chart.setDesktopNotifyEnabled(b); setAlertParam('desktop_notify_enabled', b); }}
                ariaLabel="toggle desktop notify"
              />
            </Parameter>
            <Parameter
              className="flex items-center justify-between gap-2 text-xs text-text-2"
              title="两次告警之间至少间隔多少秒，跨分组共同使用。例如 60 表示一分钟内不重复发出告警；0 表示不限制。会影响告警发送，不是图表显示设置。"
            >
              <span>冷却秒数</span>
              <input
                type="number" min={0} max={3600} step={10}
                className="w-16 px-2 py-1 text-right"
                style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
                value={chart.cooldownSeconds}
                onChange={(e) => {
                  const n = Number(e.target.value);
                  chart.setCooldownSeconds(n);
                  setAlertParam('cooldown_seconds', n);
                }}
              />
            </Parameter>

          </div>
        </Card>

        <Card
          title="候选（Candidate）"
          right={
            <Switch
              checked={chart.candidateEnabled}
              onCheckedChange={(b) => { chart.setCandidateEnabled(b); }}
              ariaLabel="toggle candidate master"
            />
          }
        >
          <div className="flex flex-col gap-2">
            <Parameter className="flex items-center justify-between gap-2 text-xs text-text-2" title="开启后只展示已完成低周期反转验证的候选，也包括后来到期的历史已验证记录。关闭后也能查看尚未验证的候选；不改变检测规则。">
              <span>仅展示 Validated</span>
              <Switch
                checked={chart.candidateValidatedOnly}
                onCheckedChange={(b) => { chart.setCandidateValidatedOnly(b); }}
                ariaLabel="toggle candidate validated only"
              />
            </Parameter>
            <p className="text-[11px] leading-4 text-text-3">
              包含曾完成验证、随后进入 Expired 的历史 Candidate。
            </p>
          </div>
        </Card>

        <Card
          title="CISD"
          right={
            <Switch
              checked={d.cisdEnabled}
              onCheckedChange={(b) => { d.setCisdEnabled(b); }}
              ariaLabel="toggle cisd"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2"
            title="同向价格推进至少持续多少根 K 线，才作为可检测的一段走势。随后价格反向收盘越过该段起始 K 线开盘价，才确认 CISD。调大更严格，会重新检测。"
          >
            <span>最少推进根数</span>
            <input
              type="number" min={1} max={10} step={1}
              className="w-16 px-2 py-1 text-right"
              style={{
                background: 'var(--bg-3)',
                border: '1px solid var(--border)',
                color: 'var(--text-1)',
                borderRadius: 3,
              }}
              value={d.cisdMinLegBars}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setCisdMinLegBars(n);
                setParam('cisd', 'min_leg_bars', n);
              }}
            />
          </Parameter>
        </Card>

        <Card
          title="MSS"
          right={
            <Switch
              checked={d.mssEnabled}
              onCheckedChange={(b) => { d.setMssEnabled(b); }}
              ariaLabel="toggle mss"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2"
            title="转折点确认根数 N：一个高点需高于左右各 N 根 K 线，低点相反。N 越大，转折越少、确认越慢；需等右侧 N 根走完。改变此值会重新检测市场结构。"
          >
            <span>转折点确认根数</span>
            <input
              type="number" min={1} max={5} step={1}
              className="w-16 px-2 py-1 text-right"
              style={{
                background: 'var(--bg-3)',
                border: '1px solid var(--border)',
                color: 'var(--text-1)',
                borderRadius: 3,
              }}
              value={d.mssFractalN}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setMssFractalN(n);
                setParam('mss', 'fractal_n', n);
              }}
            />
          </Parameter>
        </Card>

        <Card
          title="FVG"
          right={
            <Switch
              checked={d.fvgEnabled}
              onCheckedChange={(b) => {
                d.setFvgEnabled(b);
              }}
              ariaLabel="toggle fvg"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2"
            title="缺口上下边界至少相差多少 pips。EURUSD 的 1 pip = 0.0001；0 不按大小过滤。调高保留的缺口更少，会重新检测。"
          >
            <span>最小缺口（pips）</span>
            <input
              type="number"
              min={0}
              step={0.5}
              className="w-20 px-2 py-1 text-right"
              style={{
                background: 'var(--bg-3)',
                border: '1px solid var(--border)',
                color: 'var(--text-1)',
                borderRadius: 3,
              }}
              value={d.fvgMinSizePips}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setFvgMinSizePips(n);
                setParam('fvg', 'min_size_pips', n);
              }}
            />
          </Parameter>
          <div className="flex flex-col gap-1 mt-1">
            {(Object.keys(FVG_STATE_LABELS) as Fvg['state'][]).map((st) => (
              <Parameter
                key={st}
                className="flex items-center gap-2 cursor-pointer"
                title={FVG_STATE_LABELS[st].hint}
              >
                <input
                  type="checkbox"
                  checked={d.fvgStates.has(st)}
                  onChange={() => d.toggleFvgState(st)}
                />
                <span>{FVG_STATE_LABELS[st].label}</span>
              </Parameter>
            ))}
          </div>
        </Card>

        <Card
          title="PDH / PDL"
          right={
            <Switch
              checked={d.pdhPdlEnabled}
              onCheckedChange={(b) => { d.setPdhPdlEnabled(b); }}
              ariaLabel="toggle pdh_pdl"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2 text-sm"
            title="选择何时开始新的一天：纽约午夜 00:00，或纽约 17:00。会改变昨日高低价的统计边界并重新计算；纽约夏令时自动换算。"
          >
            <span style={{ color: 'var(--text-2)' }}>日界</span>
            <select
              value={d.pdhPdlMode}
              onChange={(e) => {
                const v = e.target.value as 'ny_local' | 'ny_1700';
                d.setPdhPdlMode(v);
                setParam('pdh_pdl', 'daily_boundary', v);
              }}
              className="rounded px-2 py-1"
              style={{
                background: 'var(--bg-1)',
                color: 'var(--text-1)',
                border: '1px solid var(--border)',
              }}
            >
              <option value="ny_local">NY 自然日 (默认)</option>
              <option value="ny_1700">17:00 NY (ICT 原版)</option>
            </select>
          </Parameter>
        </Card>

        <Card
          title="Liquidity"
          right={
            <Switch
              checked={d.liquidityEnabled}
              onCheckedChange={(b) => { d.setLiquidityEnabled(b); }}
              ariaLabel="toggle liquidity"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2"
            title="显示价格突破转折高点或低点后又收回的扫单。BSL 是高点上方，SSL 是低点下方。此开关只影响显示。"
          >
            <span>Swing BSL / SSL</span>
            <input type="checkbox" checked={d.liquiditySwingSweeps} onChange={(e) => d.setLiquiditySwingSweeps(e.target.checked)} />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="显示价格接近的两个高点（EQH，等高）或低点（EQL，等低），以及后来被价格扫过的状态。此开关只影响显示。"
          >
            <span>EQH / EQL</span>
            <input type="checkbox" checked={d.liquidityEqhEql} onChange={(e) => d.setLiquidityEqhEql(e.target.checked)} />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="显示突破昨日最高价后收回，或跌破昨日最低价后收回的扫单。此开关只影响显示。"
          >
            <span>PDH / PDL sweep</span>
            <input type="checkbox" checked={d.liquidityPdhPdlSweeps} onChange={(e) => d.setLiquidityPdhPdlSweeps(e.target.checked)} />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="判断两个高点或低点是否接近的容差，按平均真实波幅 ATR 的倍数计算，并受下方最大点数限制。越大越容易视为等高/等低；修改会重新检测。">
            <span>等高低容差（ATR 倍数）</span>
            <input
              type="number" min={0} max={1} step={0.01}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.liquidityEqToleranceAtrMult}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setLiquidityEqToleranceAtrMult(n);
                setParam('liquidity', 'eq_tolerance_atr_mult', n);
              }}
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="等高/等低允许价差的最大点数，与 ATR 容差共同限制。越小越要求价格接近。外汇 EURUSD 的 1 pip = 0.0001；修改会重新检测。">
            <span>最大容差（pips）</span>
            <input
              type="number" min={0.1} max={20} step={0.1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.liquidityEqToleranceMaxPips}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setLiquidityEqToleranceMaxPips(n);
                setParam('liquidity', 'eq_tolerance_max_pips', n);
              }}
            />
          </Parameter>
        </Card>

        <Card
          title="Liquidity Reversal"
          right={
            <Switch
              checked={d.liquidityReversalEnabled}
              onCheckedChange={(b) => { d.setLiquidityReversalEnabled(b); }}
              ariaLabel="toggle liquidity reversal"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2"
            title="允许扫过高低点后，用反向收盘越过一段走势起始开盘价（CISD）确认反转。关闭后此类确认不参与检测；不是隐藏图标。"
          >
            <span>允许 CISD 确认</span>
            <input
              type="checkbox"
              checked={d.liquidityReversalCisd}
              onChange={(e) => { d.setLiquidityReversalCisd(e.target.checked); }}
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="允许扫过高低点后，用市场结构转向（MSS）确认反转。MSS 在评分中占 2 分，CISD 占 1 分。关闭会改变反转检测结果。"
          >
            <span>允许 MSS 确认</span>
            <input
              type="checkbox"
              checked={d.liquidityReversalMss}
              onChange={(e) => { d.setLiquidityReversalMss(e.target.checked); }}
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="价格扫过高低点后，最多等多少根当前周期 K 线出现反转确认。例如 5 分钟周期的 10 根约为 50 分钟；越大允许确认越晚。修改会重新检测。"
          >
            <span>最多等待根数</span>
            <input
              type="number" min={1} max={50} step={1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.liquidityReversalMaxBars}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setLiquidityReversalMaxBars(n);
                setParam('liquidity_reversal', 'max_bars_after_sweep', n);
              }}
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="反转结果必须达到的最低分数。普通转折点扫单加 1 分、等高等低加 2 分、昨日高低点加 3 分；CISD 确认加 1 分、MSS 加 2 分。阈值越高，保留结果越少；不是胜率。"
          >
            <span>最低分数</span>
            <input
              type="number" min={0} max={5} step={1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.liquidityReversalMinScore}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setLiquidityReversalMinScore(n);
                setParam('liquidity_reversal', 'min_score', n);
              }}
            />
          </Parameter>
        </Card>

        <Card
          title="Premium / Discount"
          right={
            <Switch
              checked={d.premiumDiscountEnabled}
              onCheckedChange={(b) => { d.setPremiumDiscountEnabled(b); }}
              ariaLabel="toggle premium discount"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2"
            title="显示最近高低点价格区间的正中间水平线（EQ）。开启显示、关闭隐藏，不改变后台检测。"
          >
            <span>显示区间中线</span>
            <Switch
              checked={d.premiumDiscountShowEqLine}
              onCheckedChange={(b) => { d.setPremiumDiscountShowEqLine(b); }}
              ariaLabel="toggle premium discount eq line"
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="显示中线以上的高价区和以下的低价区背景。关闭只隐藏背景，不改变价格位置的计算。"
          >
            <span>显示高价 / 低价区域</span>
            <Switch
              checked={d.premiumDiscountShowZones}
              onCheckedChange={(b) => { d.setPremiumDiscountShowZones(b); }}
              ariaLabel="toggle premium discount zones"
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="价格靠近高低区间中点时，允许偏离多少 pips 仍视为中间区域。越大，中间区域越宽；修改会重新计算价格所处位置。"
          >
            <span>中线容差（pips）</span>
            <input
              type="number" min={0} max={50} step={0.1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.premiumDiscountTolerancePips}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setPremiumDiscountTolerancePips(n);
                setParam('premium_discount', 'equilibrium_tolerance_pips', n);
              }}
            />
          </Parameter>
          <Help text="当前使用最近一组已确认的转折高点和低点作为区间，暂不支持选择其他区间来源。"><span tabIndex={0}>
            区间来源：最近转折高低点
          </span></Help>
        </Card>

        <Card
          title="Sessions / Kill Zones"
          right={
            <Switch
              checked={d.sessionsEnabled}
              onCheckedChange={(b) => { d.setSessionsEnabled(b); }}
              ariaLabel="toggle sessions"
            />
          }
        >
          <Help text="以下时段按纽约当地时间计算，夏令时由系统换算；不是北京时间。仅控制图表时段显示。"><span tabIndex={0}>
            按纽约时间划分，图表显示北京时间
          </span></Help>
          <Parameter className="flex items-center justify-between gap-2" title="用矩形标出该交易时段，矩形上下沿是时段内最高价和最低价。关闭只隐藏矩形。">
            <span>显示交易时段区域</span>
            <Switch
              checked={d.sessionsShowBoxes}
              onCheckedChange={(b) => { d.setSessionsShowBoxes(b); }}
              ariaLabel="toggle session boxes"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="在交易时段矩形内显示名称，方便区分亚洲、伦敦、纽约等时段。关闭只隐藏名称。">
            <span>显示时段名称</span>
            <Switch
              checked={d.sessionsShowLabels}
              onCheckedChange={(b) => { d.setSessionsShowLabels(b); }}
              ariaLabel="toggle session labels"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="用额外虚线标出交易时段最高价和最低价。矩形边框也表达相同位置；关闭只隐藏辅助线。">
            <span>显示时段高低辅助线</span>
            <Switch
              checked={d.sessionsShowHighLow}
              onCheckedChange={(b) => { d.setSessionsShowHighLow(b); }}
              ariaLabel="toggle session high low"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="加深交易时段矩形的底色，让时段更容易看清；只改变显示，底色仍限制在该时段高低价范围内。">
            <span>增强区域底色</span>
            <Switch
              checked={d.sessionsShowBackground}
              onCheckedChange={(b) => { d.setSessionsShowBackground(b); }}
              ariaLabel="toggle session background"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="显示亚洲时段：纽约时间 20:00 至次日 00:00。夏令时自动换算；关闭只隐藏该时段。">
            <span>Asia</span>
            <Switch
              checked={d.sessionAsiaEnabled}
              onCheckedChange={(b) => { d.setSessionAsiaEnabled(b); }}
              ariaLabel="toggle asia session"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="显示伦敦开盘时段：纽约时间 02:00–05:00。夏令时自动换算；关闭只隐藏该时段。">
            <span>London Open</span>
            <Switch
              checked={d.sessionLondonOpenEnabled}
              onCheckedChange={(b) => { d.setSessionLondonOpenEnabled(b); }}
              ariaLabel="toggle london open session"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="显示纽约开盘时段：纽约时间 07:00–10:00。夏令时自动换算；关闭只隐藏该时段。">
            <span>NY Open</span>
            <Switch
              checked={d.sessionNewYorkOpenEnabled}
              onCheckedChange={(b) => { d.setSessionNewYorkOpenEnabled(b); }}
              ariaLabel="toggle new york open session"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="显示伦敦收盘时段：纽约时间 10:00–12:00。夏令时自动换算；关闭只隐藏该时段。">
            <span>London Close</span>
            <Switch
              checked={d.sessionLondonCloseEnabled}
              onCheckedChange={(b) => { d.setSessionLondonCloseEnabled(b); }}
              ariaLabel="toggle london close session"
            />
          </Parameter>
        </Card>

        <Card
          title="Power of 3 / AMD"
          right={
            <Switch
              checked={d.po3Enabled}
              onCheckedChange={(b) => { d.setPo3Enabled(b); setParam('po3', 'enabled', b); }}
              ariaLabel="toggle po3"
            />
          }
        >
          <Parameter className="flex items-center justify-between gap-2" title="显示三阶段形态的方向箭头及确认标记，便于定位发生时间。关闭只隐藏标记。">
            <span>显示三阶段确认标记</span>
            <Switch
              checked={d.po3ShowMarkers}
              onCheckedChange={(b) => { d.setPo3ShowMarkers(b); }}
              ariaLabel="toggle po3 markers"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="显示已确认形态的横盘蓄势、扫单试探、方向展开三个阶段区域。下方可分别控制各阶段；只影响显示。">
            <span>显示三个阶段区域</span>
            <Switch
              checked={d.po3ShowStageBoxes}
              onCheckedChange={(b) => { d.setPo3ShowStageBoxes(b); }}
              ariaLabel="toggle po3 stage boxes"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="显示价格在较窄区间来回波动的横盘蓄势区域；需同时开启阶段区域总开关，只影响显示。">
            <span>显示横盘蓄势阶段</span>
            <Switch
              checked={d.po3ShowAccumulationStage}
              onCheckedChange={(b) => { d.setPo3ShowAccumulationStage(b); }}
              ariaLabel="toggle po3 accumulation stage"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="显示价格扫过横盘边界的试探区域；需同时开启阶段区域总开关，只影响显示。">
            <span>显示扫单试探阶段</span>
            <Switch
              checked={d.po3ShowManipulationStage}
              onCheckedChange={(b) => { d.setPo3ShowManipulationStage(b); }}
              ariaLabel="toggle po3 manipulation stage"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="显示扫单后价格向确认方向展开的区域；需同时开启阶段区域总开关，只影响显示。">
            <span>显示方向展开阶段</span>
            <Switch
              checked={d.po3ShowDistributionStage}
              onCheckedChange={(b) => { d.setPo3ShowDistributionStage(b); }}
              ariaLabel="toggle po3 distribution stage"
            />
          </Parameter>
          <div className="mt-2 space-y-1" >
            <span className="block" style={{ color: 'var(--text-2)' }}>检测周期（周线仅作背景参考）</span>
            <div className="grid grid-cols-4 gap-1">
              {(['1m', '5m', '15m', '30m', '1h', '4h', '1d'] as const).map((tf) => {
                const checked = tf === '1m' ? d.po3ExecTf1m : tf === '5m' ? d.po3ExecTf5m : tf === '15m' ? d.po3ExecTf15m : tf === '30m' ? d.po3ExecTf30m : tf === '1h' ? d.po3ExecTf1h : tf === '4h' ? d.po3ExecTf4h : d.po3ExecTf1d;
                return (
                  <Parameter title={`在 ${tf} 周期检测三阶段形态。开启增加该周期的检测，关闭停止该周期检测；会重新计算历史结果。`} key={tf} className="flex items-center gap-1">
                    <Switch
                      checked={checked}
                      onCheckedChange={(b) => { d.setPo3ExecTf(tf, b); setParam('po3', `enabled_${tf}`, b); }}
                      ariaLabel={`toggle po3 ${tf}`}
                    />
                    <span>{tf}</span>
                  </Parameter>
                );
              })}
            </div>
          </div>
          <Parameter className="flex items-center justify-between gap-2" title="横盘蓄势阶段至少持续多少根检测周期 K 线。例如 5 分钟周期的 6 根约 30 分钟。越大越排除短暂横盘；修改会重新检测历史。">
            <span>横盘最少根数</span>
            <input
              type="number" min={2} max={100} step={1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.po3MinAccumulationBars}
              onChange={(e) => { const n = Number(e.target.value); d.setPo3MinAccumulationBars(n); setParam('po3', 'min_accumulation_bars', n); }}
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="横盘蓄势阶段最多纳入多少根检测周期 K 线。越大允许更长的横盘；应不小于最少根数。修改会重新检测历史。">
            <span>横盘最多根数</span>
            <input
              type="number" min={2} max={200} step={1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.po3MaxAccumulationBars}
              onChange={(e) => { const n = Number(e.target.value); d.setPo3MaxAccumulationBars(n); setParam('po3', 'max_accumulation_bars', n); }}
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="横盘高低差最多为最近 14 根平均真实波幅 ATR 的多少倍。越小要求横盘越窄，越大容许更宽的波动；修改会重新检测。">
            <span>横盘最大宽度（ATR 倍数）</span>
            <input
              type="number" min={0.1} max={10} step={0.1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.po3MaxRangeAtrMult}
              onChange={(e) => { const n = Number(e.target.value); d.setPo3MaxRangeAtrMult(n); setParam('po3', 'max_range_atr_mult', n); }}
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="开启后，横盘区必须出现接近的高低点或边界多次触碰，才认为有可扫的流动性。关闭会放宽检测条件；会重新检测。">
            <span>要求存在流动性聚集</span>
            <Switch
              checked={d.po3RequireLiquidityPool}
              onCheckedChange={(b) => { d.setPo3RequireLiquidityPool(b); setParam('po3', 'require_liquidity_pool', b); }}
              ariaLabel="toggle po3 liquidity pool"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="扫过横盘边界后，最多等待多少根检测周期 K 线出现 CISD 或 MSS 反转确认。越大允许确认更晚；修改会重新检测。">
            <span>扫单后最多等待根数</span>
            <input
              type="number" min={1} max={100} step={1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.po3MaxBarsAfterSweep}
              onChange={(e) => { const n = Number(e.target.value); d.setPo3MaxBarsAfterSweep(n); setParam('po3', 'max_bars_after_sweep', n); }}
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="三阶段形态的质量分必须达到此值才保留。越高越严格、信号通常越少；质量分不是盈利概率。修改会重新检测。">
            <span>最低质量分</span>
            <input
              type="number" min={1} max={10} step={1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.po3MinQualityScore}
              onChange={(e) => { const n = Number(e.target.value); d.setPo3MinQualityScore(n); setParam('po3', 'min_quality_score', n); }}
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="允许用反向收盘越过走势起始开盘价（CISD）确认三阶段形态的转向。关闭会排除此类确认；修改会重新检测。">
            <span>允许 CISD 确认</span>
            <Switch
              checked={d.po3AllowCisd}
              onCheckedChange={(b) => { d.setPo3AllowCisd(b); setParam('po3', 'allow_cisd', b); }}
              ariaLabel="toggle po3 cisd"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="允许用市场结构转向（MSS）确认三阶段形态的转向。关闭会排除此类确认；修改会重新检测。">
            <span>允许 MSS 确认</span>
            <Switch
              checked={d.po3AllowMss}
              onCheckedChange={(b) => { d.setPo3AllowMss(b); setParam('po3', 'allow_mss', b); }}
              ariaLabel="toggle po3 mss"
            />
          </Parameter>
        </Card>

        <Card
          title="BoS"
          right={
            <Switch
              checked={d.bosEnabled}
              onCheckedChange={(b) => { d.setBosEnabled(b); }}
              ariaLabel="toggle bos"
            />
          }
        >
          <Help text="Break of Structure：顺势 close 突破最近 swing；显示层开关，不清后端结构。"><span tabIndex={0}>
            显示顺势结构突破标记
          </span></Help>
        </Card>

        <Card
          title="Order Block"
          right={
            <Switch
              checked={d.obEnabled}
              onCheckedChange={(b) => { d.setObEnabled(b); }}
              ariaLabel="toggle ob"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2"
            title="订单块形成后，价格离开时所需的力度，以最近 14 根 K 线的平均真实波幅（ATR）为单位。1.5 表示 1.5 倍 ATR；越大要求越强，符合条件的订单块通常越少。修改会重新检测。"
          >
            <span>离开力度（ATR 倍数）</span>
            <input
              type="range"
              min={0.5} max={3.0} step={0.1}
              value={d.obDisplacementAtrMult}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setObDisplacementAtrMult(n);
                setParam('order_block', 'displacement_atr_mult', n);
              }}
              style={{ width: 120 }}
            />
            <span style={{ minWidth: 32, textAlign: 'right' }}>
              {d.obDisplacementAtrMult.toFixed(1)}
            </span>
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="开启：只把开盘价到收盘价之间的实体作为订单块范围。关闭：连同上下影线，使用最低价到最高价。开启后的区间通常更窄；会重新检测。"
          >
            <span>仅使用 K 线实体</span>
            <Switch
              checked={d.obUseBodyOnly}
              onCheckedChange={(b) => {
                d.setObUseBodyOnly(b);
                setParam('order_block', 'use_body_only', b);
              }}
              ariaLabel="ob body only"
            />
          </Parameter>
        </Card>

        <Card
          title="Breaker Block"
          right={
            <Switch
              checked={d.breakerEnabled}
              onCheckedChange={(b) => { d.setBreakerEnabled(b); }}
              ariaLabel="toggle breaker block"
            />
          }
        >
          <Help text="OB 失效后反向使用的区间；显示层开关，不清后端结构。"><span tabIndex={0}>
            显示有效 / 已回测的反向区域
          </span></Help>
        </Card>

        <Card
          title="OTE"
          right={
            <Switch
              checked={d.oteEnabled}
              onCheckedChange={(b) => { d.setOteEnabled(b); }}
              ariaLabel="toggle ote"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2"
            title="回撤区下边界比例：0.62 表示一段高低点价差的 62%。应小于上限；修改会重新计算回撤区域，不代表必然在此反转。"
          >
            <span>回撤比例下限</span>
            <input
              type="number" min={0.1} max={0.95} step={0.01}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.oteFibLow}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setOteFibLow(n);
                setParam('ote', 'fib_low', n);
              }}
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="回撤区上边界比例：0.79 表示一段高低点价差的 79%。应大于下限；上下限差越大，区域越宽。修改会重新计算。"
          >
            <span>回撤比例上限</span>
            <input
              type="number" min={0.1} max={0.95} step={0.01}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.oteFibHigh}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setOteFibHigh(n);
                setParam('ote', 'fib_high', n);
              }}
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="只用最近 N 根 K 线内仍有效的缺口、订单块等区域，检查它们是否与回撤区重叠。数值越大，纳入比较的历史越多；修改会重新计算。"
          >
            <span>重叠检查根数</span>
            <input
              type="number" min={20} max={1000} step={10}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.oteConfluenceLookbackBars}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setOteConfluenceLookbackBars(n);
                setParam('ote', 'confluence_lookback_bars', n);
              }}
            />
          </Parameter>
        </Card>

        <Card
          title="Opening Gaps"
          right={
            <Switch
              checked={d.openingGapsEnabled}
              onCheckedChange={(b) => { d.setOpeningGapsEnabled(b); }}
              ariaLabel="toggle opening gaps"
            />
          }
        >
          <Help text="NWOG/NDOG 只在 NY 新周/新日边界形成；filled 默认隐藏，验收历史缺口时请打开 show filled。"><span tabIndex={0}>
            在纽约新日 / 新周边界形成，完全回补默认隐藏
          </span></Help>
          <Parameter className="flex items-center justify-between gap-2" title="显示新周开盘缺口。显示层开关，不触发后端重算。">
            <span>显示周开盘缺口</span>
            <Switch
              checked={d.openingGapsShowNwog}
              onCheckedChange={(b) => { d.setOpeningGapsShowNwog(b); }}
              ariaLabel="toggle nwog"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="显示新日开盘缺口。显示层开关，不触发后端重算。">
            <span>显示日开盘缺口</span>
            <Switch
              checked={d.openingGapsShowNdog}
              onCheckedChange={(b) => { d.setOpeningGapsShowNdog(b); }}
              ariaLabel="toggle ndog"
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="显示尚未回补到 50% 的 NWOG / NDOG。"
          >
            <span>显示未回补到中线的缺口</span>
            <Switch
              checked={d.openingGapsShowActive}
              onCheckedChange={(b) => { d.setOpeningGapsShowActive(b); }}
              ariaLabel="toggle opening gap active"
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="显示已经回补到 gap 中线，但尚未完全填补的 NWOG / NDOG。"
          >
            <span>显示部分回补的缺口</span>
            <Switch
              checked={d.openingGapsShowMitigated}
              onCheckedChange={(b) => { d.setOpeningGapsShowMitigated(b); }}
              ariaLabel="toggle opening gap mitigated"
            />
          </Parameter>
          <Parameter
            className="flex items-center justify-between gap-2"
            title="显示已经完全回补的 NWOG / NDOG；默认关闭，避免历史缺口遮挡图表。"
          >
            <span>显示完全回补的缺口</span>
            <Switch
              checked={d.openingGapsShowFilled}
              onCheckedChange={(b) => { d.setOpeningGapsShowFilled(b); }}
              ariaLabel="toggle opening gap filled"
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="新周开盘与上一交易时段收盘之间，至少相差多少 pips 才算周开盘缺口。0 不按大小过滤；越大保留的缺口越少，会重新检测。">
            <span>周缺口最小值（pips）</span>
            <input
              type="number" min={0} max={100} step={0.1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.openingGapsMinNwogSizePips}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setOpeningGapsMinNwogSizePips(n);
                setParam('opening_gap', 'min_nwog_size_pips', n);
              }}
            />
          </Parameter>
          <Parameter className="flex items-center justify-between gap-2" title="新日开盘与上一交易时段收盘之间，至少相差多少 pips 才算日开盘缺口。0 不按大小过滤；越大保留的缺口越少，会重新检测。">
            <span>日缺口最小值（pips）</span>
            <input
              type="number" min={0} max={100} step={0.1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.openingGapsMinNdogSizePips}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setOpeningGapsMinNdogSizePips(n);
                setParam('opening_gap', 'min_ndog_size_pips', n);
              }}
            />
          </Parameter>
        </Card>

        <Card
          title="Volume Imbalance"
          right={
            <Switch
              checked={d.viEnabled}
              onCheckedChange={(b) => { d.setViEnabled(b); }}
              ariaLabel="toggle volume imbalance"
            />
          }
        >
          <Parameter
            className="flex items-center justify-between gap-2"
            title="相邻两根 K 线实体之间的空隙至少多大才保留，以 pips 为单位。EURUSD 的 1 pip = 0.0001；越大保留结果越少，修改会重新检测。"
          >
            <span>最小缺口（pips）</span>
            <input
              type="number" min={0} max={10} step={0.1}
              className="w-16 px-2 py-1 text-right"
              style={{ background: 'var(--bg-3)', border: '1px solid var(--border)', color: 'var(--text-1)', borderRadius: 3 }}
              value={d.viMinSizePips}
              onChange={(e) => {
                const n = Number(e.target.value);
                d.setViMinSizePips(n);
                setParam('volume_imbalance', 'min_size_pips', n);
              }}
            />
          </Parameter>
        </Card>
     </div>
    </div>
  );
}
