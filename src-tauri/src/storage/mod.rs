//! Persistent storage layer (SQLite, with an in-memory layer to follow later).

pub mod sqlite;

pub use sqlite::SqliteStore;
