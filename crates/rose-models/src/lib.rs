//! Model clients (IMPLEMENTATION.md T3.1).
//!
//! Phase 1 has exactly one real call: screenshot + goal + short history
//! into Muse Spark, one JSON action out. Everything Meta-API-specific
//! lives in [`spark`]; [`fake`] is the scripted test double.

pub mod fake;
pub mod spark;

pub use fake::FakeBrain;
pub use spark::{ModelReply, MuseClient};
