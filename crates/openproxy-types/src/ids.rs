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

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
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

        impl From<$name> for i64 {
            fn from(id: $name) -> Self {
                id.0
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

/// Deterministically converts a 64-bit integer into an RFC 4122 compliant UUID v4.
pub fn u64_to_v4_uuid(h: u64) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&h.to_be_bytes());
    let h2 = h ^ 0xa5a5_a5a5_a5a5_a5a5;
    bytes[8..16].copy_from_slice(&h2.to_be_bytes());
    // RFC 4122 Version 4 (0100 in bits 12-15 of time_hi_and_version)
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // RFC 4122 Variant 1 (10 in bits 6-7 of clock_seq_hi_and_reserved)
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// Helper to convert any session or thread identifier deterministically into an RFC 4122 compliant UUID v4 string.
pub fn format_as_v4_uuid(input: &str) -> String {
    let trimmed = input.trim().trim_matches('"');
    if let Ok(parsed) = Uuid::parse_str(trimmed)
        && parsed.get_variant() == uuid::Variant::RFC4122
        && parsed.get_version().is_some()
    {
        return parsed.to_string();
    }
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(trimmed, &mut hasher);
    let h = std::hash::Hasher::finish(&hasher);
    u64_to_v4_uuid(h).to_string()
}

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

    #[test]
    fn string_id_conversions_and_deref() {
        let p_from_str: ProviderId = "openai".into();
        let p_from_string: ProviderId = String::from("openai").into();
        assert_eq!(p_from_str, p_from_string);
        assert_eq!(&*p_from_str, "openai");
        assert_eq!(p_from_str.len(), 6);
        let s: String = p_from_str.into();
        assert_eq!(s, "openai");
    }

    #[test]
    fn numeric_id_conversions() {
        let acc: AccountId = 42.into();
        let val: i64 = acc.into();
        assert_eq!(val, 42);
    }

    #[test]
    fn string_id_borrow_and_hashmap() {
        use std::borrow::Borrow;
        use std::collections::HashMap;

        let mut map = HashMap::new();
        let pid = ProviderId::new("openrouter");
        map.insert(pid.clone(), 123);

        assert_eq!(map.get("openrouter"), Some(&123));
        let borrowed: &str = pid.borrow();
        assert_eq!(borrowed, "openrouter");
    }

    #[test]
    fn test_u64_to_v4_uuid_rfc4122_compliance() {
        let test_cases = [0u64, 1, 42, u64::MAX, 0x1234_5678_9abc_def0];
        for val in test_cases {
            let u = u64_to_v4_uuid(val);
            assert_eq!(u.get_version(), Some(uuid::Version::Random));
            assert_eq!(u.get_variant(), uuid::Variant::RFC4122);
            let s = u.to_string();
            assert_eq!(s.len(), 36);
            assert_eq!(&s[14..15], "4");
            let c8 = s.chars().nth(19).unwrap();
            assert!(c8 == '8' || c8 == '9' || c8 == 'a' || c8 == 'b');
        }
    }

    #[test]
    fn test_format_as_v4_uuid_stability_and_validity() {
        // String that is already a valid UUID v4
        let valid_v4 = "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d";
        assert_eq!(format_as_v4_uuid(valid_v4), valid_v4);

        // Arbitrary non-UUID string
        let s = "sess-openproxy-abc-123";
        let formatted = format_as_v4_uuid(s);
        let u = Uuid::parse_str(&formatted).expect("valid uuid");
        assert_eq!(u.get_version(), Some(uuid::Version::Random));
        assert_eq!(u.get_variant(), uuid::Variant::RFC4122);

        // Idempotence: formatting the formatted UUID returns the same
        assert_eq!(format_as_v4_uuid(&formatted), formatted);
    }

    #[test]
    fn test_format_as_v4_uuid_quoted_and_determinism() {
        let quoted_v4 = "\"9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d\"";
        assert_eq!(
            format_as_v4_uuid(quoted_v4),
            "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d"
        );

        let input = "same-input-string";
        let u1 = format_as_v4_uuid(input);
        let u2 = format_as_v4_uuid(input);
        assert_eq!(u1, u2);

        let u64_val = 123456789u64;
        assert_eq!(u64_to_v4_uuid(u64_val), u64_to_v4_uuid(u64_val));
    }
}
