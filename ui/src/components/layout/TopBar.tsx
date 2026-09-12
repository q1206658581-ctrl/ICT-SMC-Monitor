import { Loader2 } from 'lucide-react';
import { WatchlistBar } from './WatchlistBar';
import { useChartStore } from '../../store';

export function TopBar() {
  const loadStatus = useChartStore((s) => s.loadStatus);
  return (
    <div
      className="flex items-center justify-between px-3 select-none"
      style={{
        height: 44,
        background: 'var(--bg-1)',
        borderBottom: '1px solid var(--border)',
      }}
    >
      <div className="flex items-center gap-4 min-w-0">
        <div className="flex items-center gap-2 shrink-0">
          <div
            className="w-2 h-2 rounded-full"
            title={loadStatus === 'ready' ? '数据已连接' : '正在连接或加载数据'}
            style={{ background: loadStatus === 'ready' ? 'var(--bull)' : '#f5a623' }}
          />
          <span className="text-md font-semibold text-text-1">ICT Radar</span>
        </div>
        <div className="min-w-0 flex-1">
          <WatchlistBar />
        </div>
      </div>
      <div className="flex items-center gap-2 shrink-0">
        {loadStatus !== 'ready' && (
          <div
            className="flex items-center gap-1.5 px-2 py-1 rounded text-xs"
            style={{ background: 'var(--bg-2)', color: 'var(--text-2)' }}
          >
            <Loader2 size={12} className="animate-spin" />
            <span>
              {loadStatus === 'connecting' && 'Connecting…'}
              {loadStatus === 'loading_bars' && 'Loading bar data…'}
              {loadStatus === 'loading_indicators' && 'Loading indicators…'}
            </span>
          </div>
        )}
      </div>
    </div>
  );
}
