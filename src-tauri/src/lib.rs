//! ict-monitor library crate.
//!
//! Wires together the data-source layer, persistence, and (later) the ICT
//! detection engine. M1 only exposes the data-source skeleton + SQLite store.

pub mod aggregator;
pub mod alert;
pub mod candidate;
pub mod config;
pub mod data_source;
pub mod detector;
pub mod ipc;
pub mod llm;
pub mod storage;
pub mod types;
pub mod watchlist;
