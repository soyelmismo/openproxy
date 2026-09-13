//! Native, in-process reversible PII redaction and restoration module.

pub mod engine;
pub mod replacer;
pub mod session;

pub use engine::{PiiEngine, luhn_check};
pub use replacer::{PiiRestorationStage, StreamingWindowReplacer};
pub use session::PiiSession;

#[cfg(test)]
mod tests;
