//! Combos: ordered list of targets with a strategy. Priority or round-robin.
//! Each target references a (provider, model, optional account). Accounts can be rotated within a provider.

use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use rusqlite::OptionalExtension;

pub mod crud;


pub use crud::*;


pub use openproxy_types::combos::*;
pub use crate::config::CooldownMode;
