// M5 watchlist domain types — mirror of src-tauri/src/watchlist.rs.

export type Correlation = 'positive' | 'negative';

export type PairCorrelation = {
  a: string;
  b: string;
  direction: Correlation;
};

export type Watchlist = {
  id: string;
  name: string;
  symbols: string[];
  correlations: PairCorrelation[];
};

export type WatchlistInput = {
  name: string;
  symbols: string[];
  correlations: PairCorrelation[];
};

export type DefaultCorrelation = {
  direction: Correlation;
  known: boolean;
};
