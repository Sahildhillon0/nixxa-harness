//! `nixxa-db` — persists Nixxa's own sessions, transcripts and action logs.
//!
//! [`SqliteStore`] is the real store (Stage 5); [`memory::MemoryStore`] keeps everything in
//! memory (`--fake`, and when the database can't be opened).

pub mod memory;
mod sqlite;

pub use sqlite::SqliteStore;
