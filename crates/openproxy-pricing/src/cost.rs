//! Cost calculation — canonical implementation re-exported from `openproxy_db::cost`.
//!
//! The old in-crate implementation (its own `compute` signature, TPS guard and
//! inline tests) was consolidated into `openproxy_db::cost`, whose `tests`
//! module (`openproxy_db::cost::tests`) covers compute, TPS, redaction and
//! record paths. Nothing consumes `openproxy_pricing::cost` directly today;
//! the re-export only keeps the historical path referenced by the
//! `openproxy_types::redaction` docs alive.

pub use openproxy_db::cost::*;
