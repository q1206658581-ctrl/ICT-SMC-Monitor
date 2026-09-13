import { useState } from 'react';
import { usePositions } from '../chart/positions/store';
import { toast } from 'sonner';
import { copyAppScreenshot } from '../../lib/screenshot';
import { Sparkles, Camera, BarChart3, Bell } from 'lucide-react';
import { TIMEFRAMES, useChartStore, useLayoutStore, type Tf } from '../../store';
import { Button } from '../ui/button';
import { cn } from '../../lib/utils';

export function ChartToolbar() {
  const [capturing, setCapturing] = useState(false);
  const positionMode = usePositions(s => s.mode);
  const setPositionMode = usePositions(s => s.setMode);
  const bottomTab = useLayoutStore((s) => s.bottomTab);
  const bottomCollapsed = useLayoutStore((s) => s.bottomCollapsed);
  const tf = useChartStore((s) => s.tf);
  const setTf = useChartStore((s) => s.setTf);
  const setBottomTab = useLayoutStore((s) => s.setBottomTab);
  const setBottomCollapsed = useLayoutStore((s) => s.setBottomCollapsed);
  const rightCollapsed = useLayoutStore((s) => s.rightCollapsed);
  const setRightCollapsed = useLayoutStore((s) => s.setRightCollapsed);
  return (
    <div
      className="flex items-center justify-between gap-3 overflow-x-auto px-3 select-none"
      style={{
        height: 36,
        background: 'var(--bg-1)',
        borderBottom: '1px solid var(--border)',
      }}
    >
      <div
        className="flex shrink-0 items-center gap-1 rounded-sm p-0.5"
        style={{ background: 'var(--bg-2)' }}
      >
        {TIMEFRAMES.map((t) => (
          <button
            key={t}
            type="button"
            onClick={() => setTf(t as Tf)}
            className={cn(
              'h-6 px-2 rounded-sm text-sm transition-colors',
              tf === t
                ? 'text-text-1'
                : 'text-text-2 hover:text-text-1',
            )}
            style={
              tf === t
                ? { background: 'var(--bg-3)' }
                : undefined
            }
          >
            {t.toUpperCase()}
          </button>
        ))}
      </div>
      <div className="flex shrink-0 items-center gap-1">
        {(['long', 'short'] as const).map(side => <Button key={side} size="toolbar" variant={positionMode === side ? 'default' : 'ghost'}
          aria-pressed={positionMode === side} title="点击后在图表放置；默认间距仅作视觉参考，可拖动调整；Esc 取消"
          onClick={() => setPositionMode(positionMode === side ? null : side)}>
          {side === 'long' ? '⊕ 多头仓位' : '⊖ 空头仓位'}
        </Button>)}
        <Button
          size="toolbar"
          variant={rightCollapsed ? "ghost" : "default"}
          onClick={() => setRightCollapsed(!rightCollapsed)}
          aria-pressed={!rightCollapsed}
        >
          <BarChart3 size={14} /> 指标
        </Button>
        <Button size="toolbar" variant={!bottomCollapsed && bottomTab === 'candidates' ? 'default' : 'ghost'}
          aria-pressed={!bottomCollapsed && bottomTab === 'candidates'}
          onClick={() => { setBottomTab('candidates'); setBottomCollapsed(false); }}>
          <Bell size={14} /> 查看告警
        </Button>
        <Button
          size="toolbar"
          aria-pressed={!bottomCollapsed && bottomTab === 'reports'}
          variant={!bottomCollapsed && bottomTab === 'reports' ? 'default' : 'ghost'}
          onClick={() => { setBottomTab('reports'); setBottomCollapsed(false); }}
        >
          <Sparkles size={14} /> 查看决策
        </Button>
        <Button size="icon" variant="ghost" aria-label="复制整个 APP 页面截图" title="复制当前可见的整个 APP 页面到剪贴板" disabled={capturing}
          onClick={async () => {
            setCapturing(true);
            try { await copyAppScreenshot(); toast.success('整个 APP 页面已复制到剪贴板'); }
            catch (e) { toast.error(`截图复制失败：${String(e)}`); }
            finally { setCapturing(false); }
          }}>
          <Camera size={14} />
        </Button>
      </div>
    </div>
  );
}
