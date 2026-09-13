import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { PositionRepository } from './repository';
import type { PositionPrices, PositionSide, UserPosition } from './model';
const args = (p: PositionPrices) => ({ side: p.side, entry: p.entry_price, stop: p.stop_price, target: p.target_price });
interface PositionState {
  rows: Record<string, UserPosition[]>;
  revision: number;
  mode: PositionSide | null;
  selected: string | null;
  owner: number | null;
  setMode: (side: PositionSide | null) => void;
  select: (id: string | null, pane: number | null) => void;
}
export const usePositions = create<PositionState>((set) => ({
  rows: {}, revision: 0, mode: null, selected: null, owner: null,
  setMode: mode => set({ mode, selected: null, owner: null }),
  select: (selected, owner) => set({ selected, owner }),
}));
export const positions = new PositionRepository({
  list: symbol => invoke<UserPosition[]>('list_user_positions', { symbol }),
  create: p => invoke<string>('create_user_position', { ...args(p), symbol: p.symbol, sourceAlertId: p.source_alert_id, drawnTf: p.drawn_tf }),
  update: p => invoke<UserPosition>('update_user_position', { ...args(p), id: p.id }),
  delete: id => invoke('delete_user_position', { id }),
}, () => usePositions.setState(s => ({ rows: positions.rows, revision: s.revision+1 })), message => toast.error(message));
