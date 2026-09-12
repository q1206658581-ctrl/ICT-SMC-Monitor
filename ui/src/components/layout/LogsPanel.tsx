import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';

type LogEntry = { timestamp: string; level: 'INFO' | 'WARN' | 'ERROR'; message: string };
const colors = { INFO: 'var(--text-2)', WARN: '#f5c451', ERROR: '#f87171' };

export function LogsPanel() {
  const [entries, setEntries] = useState<LogEntry[]>([]);
  const [level, setLevel] = useState('ALL');
  const [query, setQuery] = useState('');
  const [auto, setAuto] = useState(true);
  const [refresh, setRefresh] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [updated, setUpdated] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const load = async () => {
      setLoading(true);
      try {
        const rows = await invoke<LogEntry[]>('list_runtime_logs');
        if (!cancelled) {
          setEntries(rows);
          setError(null);
          setUpdated(new Date().toLocaleTimeString('zh-CN', { hour12: false }));
        }
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) {
          setLoading(false);
          if (auto) timer = setTimeout(() => void load(), 3000);
        }
      }
    };
    void load();
    return () => { cancelled = true; if (timer) clearTimeout(timer); };
  }, [auto, refresh]);

  const needle = query.trim().toLowerCase();
  const visible = entries.filter((entry) => (level === 'ALL' || entry.level === level)
    && (!needle || `${entry.timestamp} ${entry.message}`.toLowerCase().includes(needle)));

  return (
    <div className="absolute inset-0 flex flex-col text-xs">
      <div className="flex flex-wrap items-center gap-3 px-3 py-2 border-b border-border">
        <select aria-label="日志级别" value={level} onChange={(e) => setLevel(e.target.value)} className="bg-bg-2 text-text-2 rounded px-2 py-1">
          <option value="ALL">全部级别</option><option>INFO</option><option>WARN</option><option>ERROR</option>
        </select>
        <input aria-label="搜索日志" placeholder="搜索日志…" value={query} onChange={(e) => setQuery(e.target.value)} className="bg-bg-2 text-text-2 rounded px-2 py-1" />
        <label className="flex items-center gap-1 text-text-2"><input type="checkbox" checked={auto} onChange={(e) => setAuto(e.target.checked)} />自动刷新（3 秒）</label>
        <button disabled={loading} onClick={() => setRefresh((n) => n + 1)} className="rounded border border-border px-2 py-1 text-text-2 disabled:opacity-50">{loading ? '读取中…' : '刷新'}</button>
        <span className="text-text-3">{visible.length} / {entries.length} 条{updated ? ` · 更新于 ${updated}` : ''}</span>
      </div>
      <div className="px-3 py-1 text-text-3">全局运行日志 · 最新在前 · 当前文件末尾 256 KiB 内最多 1,000 条 · 敏感内容已隐藏 · 时间为北京时间</div>
      {error && <div role="alert" className="px-3 py-1 text-red-400">日志读取失败：{error}</div>}
      <div className="min-h-0 flex-1 overflow-auto font-mono">
        {visible.length === 0 ? <div className="px-3 py-3 text-text-3">{loading ? '正在读取日志…' : entries.length ? '没有符合筛选条件的日志' : '暂无运行日志'}</div> : (
          <table className="w-full text-left"><thead className="sticky top-0 bg-bg-1 text-text-3"><tr><th className="px-3 py-1 font-normal">时间</th><th className="px-2 py-1 font-normal">级别</th><th className="px-3 py-1 font-normal">内容</th></tr></thead>
            <tbody>{visible.map((entry, index) => <tr key={`${entry.timestamp}-${index}`} className="border-b border-border/30" style={{ color: colors[entry.level] }}>
              <td className="whitespace-nowrap px-3 py-1 align-top">{new Date(entry.timestamp).toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai', hour12: false })}</td>
              <td className="px-2 py-1 align-top">{entry.level}</td>
              <td className="px-3 py-1 break-all whitespace-pre-wrap">{entry.message}</td>
            </tr>)}</tbody>
          </table>
        )}
      </div>
    </div>
  );
}
