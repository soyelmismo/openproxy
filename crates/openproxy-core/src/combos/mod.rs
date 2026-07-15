//! Combos: ordered list of targets with a strategy. Priority or round-robin.
//! Each target references a (provider, model, optional account). Accounts can be rotated within a provider.

use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use rand::RngExt;
use rand::prelude::SliceRandom;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Arc;

pub mod crud;
pub mod load_balancing;
pub mod resolution;

pub use crud::*;
pub use load_balancing::*;
pub use resolution::*;

pub use openproxy_types::combos::*;
pub use crate::config::CooldownMode;
