//! Strongly-typed IDs used across the proxy.
//!
//! Wrapper types prevent mixing up, e.g., a ProviderId with an AccountId.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! impl_uuid_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

macro_rules! impl_string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Ord, PartialOrd)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

macro_rules! impl_numeric_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Ord, PartialOrd,
        )]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl $name {
            pub const fn new(v: i64) -> Self {
                Self(v)
            }
            pub const fn value(&self) -> i64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<i64> for $name {
            fn from(v: i64) -> Self {
                Self(v)
            }
        }
    };
}

impl_uuid_id!(RequestId);
impl_uuid_id!(TraceId);

impl_string_id!(ProviderId);
impl_string_id!(ModelId);

impl_numeric_id!(AccountId);
impl_numeric_id!(ComboId);
impl_numeric_id!(ComboTargetId);
impl_numeric_id!(ModelRowId);
impl_numeric_id!(UsageId);
impl_numeric_id!(ApiKeyId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_is_unique() {
        let a = RequestId::new();
        let b = RequestId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn provider_id_display() {
        let p = ProviderId::new("openrouter");
        assert_eq!(format!("{p}"), "openrouter");
    }

    #[test]
    fn model_id_serde_preserves_string() {
        let m = ModelId::new("anthropic/claude-sonnet-4");
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, "\"anthropic/claude-sonnet-4\"");
    }
}
