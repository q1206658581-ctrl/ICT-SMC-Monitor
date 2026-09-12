export type BarPayload = {
  symbol: string;
  tf: string;
  ts: number;        // epoch ms
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  closed: boolean;
};

export type SymbolMeta = {
  symbol: string;
  provider: string;
};

export type AppStatusPayload = {
  kind: string;
  message: string;
};
