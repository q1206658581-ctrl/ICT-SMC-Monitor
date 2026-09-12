//! Offline evaluation only: never linked into the production app or IPC.
#[path = "../backtest/mod.rs"]
mod backtest;

fn main() -> anyhow::Result<()> {
    backtest::cli(std::env::args().skip(1).collect())
}
