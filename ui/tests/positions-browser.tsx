// Isolated browser fixture: real chart/UI, fake IPC and synthetic data. No production database access.
import { useMemo } from 'react';
import { createRoot } from 'react-dom/client';
import { mockIPC } from '@tauri-apps/api/mocks';
import { Toaster } from 'sonner';
import { ChartPane } from '../src/components/chart/ChartPane';
import { ChartToolbar } from '../src/components/layout/ChartToolbar';
import { DrawPositionButton } from '../src/components/chart/positions/DrawPositionButton';
import { useChartStore } from '../src/store';
import type { UserPosition } from '../src/components/chart/positions/model';
import '../src/styles/tokens.css';
const key = 'drawing-browser-fixture';
const read = (): UserPosition[] => JSON.parse(localStorage.getItem(key) ?? '[]');
const write = (rows: UserPosition[]) => localStorage.setItem(key,JSON.stringify(rows));
mockIPC((command,args: Record<string,any> = {}) => {
  const rows = read();
  if (command === 'list_user_positions') return rows.filter(p => p.symbol === args.symbol);
  if (command === 'create_user_position') {
    const id = crypto.randomUUID(),now = Date.now();
    write([...rows,{ id,symbol: args.symbol,side: args.side,entry_price: args.entry,stop_price: args.stop,target_price: args.target,source_alert_id: args.sourceAlertId,drawn_tf: args.drawnTf,anchor_ts: args.sourceAlertId ? 1800000000000+50*300000 : null,created_at_ts: now,updated_at_ts: now }]); return id;
  }
  if (command === 'update_user_position') {
    const old = rows.find(p => p.id === args.id)!;
    const p = { ...old,side: args.side,entry_price: args.entry,stop_price: args.stop,target_price: args.target,updated_at_ts: Date.now() };
    write(rows.map(old => old.id === p.id ? p : old)); return p;
  }
  if (command === 'delete_user_position') { write(rows.filter(p => p.id !== args.id)); return null; }
  if (command === 'get_user_position_draft') return { symbol: 'OANDA:EURUSD',side: 'long',entry_price: 100,stop_price: 99.8,target_price: null,source_alert_id: args.alertId,drawn_tf: '5m',anchor_ts: 1800000000000+50*300000 };
  if (command.startsWith('list_')) return [];
  return null;
},{ shouldMockEvents: true });
function Pane({ symbol,index }: { symbol: string;index: number }) {
  const tf = useChartStore(s => s.tf);
  const rows = useMemo(() => Array.from({ length: 100 },(_,i) => {
    const price = (symbol === 'TVC:DXY' ? 50 : 100)+Math.sin(i/7)*0.1;
    return { symbol,tf,ts: 1800000000000+i*300000,open: price,close: price+0.02,high: price+0.05,low: price-0.04,volume: 100,closed: true };
  }),[symbol,tf]);
  return <div style={{ flex: 1,minWidth: 0,height: '100%' }} data-qa-pane={index}><ChartPane symbol={symbol} paneIndex={index} historyRows={rows} /></div>;
}
function App() {
  return <div style={{ height: '100vh',background: '#0e1114',color: '#eee' }}>
    <ChartToolbar />
    <div style={{ padding: 8 }}>隔离测试：前两图同 EURUSD，第三图 DXY · <DrawPositionButton alertId="fixture-alert" /></div>
    <div style={{ display: 'flex',height: 'calc(100vh - 85px)' }}><Pane symbol="OANDA:EURUSD" index={0} /><Pane symbol="OANDA:EURUSD" index={1} /><Pane symbol="TVC:DXY" index={2} /></div>
    <Toaster />
  </div>;
}
createRoot(document.getElementById('root')!).render(<App />);
