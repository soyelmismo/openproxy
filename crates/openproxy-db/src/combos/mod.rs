//! Combo storage and target resolution.

pub mod crud;
pub mod mapping;
pub mod resolve;
pub mod settings;
pub mod targets;

#[cfg(test)]
mod tests;

pub use crud::*;
pub use openproxy_types::combos::AddTargetInput;
pub use resolve::*;
pub use settings::*;
pub use targets::*;
