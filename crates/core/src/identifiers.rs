//! Squib-wide validated identifier newtypes.
//!
//! Per [70-security.md § 4](../../../specs/70-security.md#4-input-validation):
//! every external string crossing into squib goes through a fallible-constructor newtype
//! before reaching downstream code. Validation runs once in `new`/`try_from`; every
//! downstream use is provably safe by construction.
//!
//! Placing the canonical newtypes at `squib-core` (the bottom of the dependency DAG —
//! see [I-CRATE-1](../../../specs/61-crates-and-features.md#7-invariants)) means any crate
//! can witness the validation in the type system without cross-crate detours via
//! `squib-api`. The API layer's `Raw* → Validated TryFrom` boundary still drives the
//! validation; this module hosts the result types.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can surface while validating an identifier.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdentifierError {
    /// Value is empty when a non-empty identifier was required.
    #[error("{kind}: must not be empty")]
    Empty {
        /// Kind tag (`"host_dev_name"`, `"iface_id"`, …) for the operator-facing
        /// error message.
        kind: &'static str,
    },
    /// Value exceeds its byte cap.
    #[error("{kind}: exceeds {max} bytes (got {len} bytes)")]
    TooLong {
        /// Kind tag.
        kind: &'static str,
        /// Actual byte length.
        len: usize,
        /// Maximum allowed.
        max: usize,
    },
    /// Value contains an interior NUL byte.
    #[error("{kind}: contains a NUL byte")]
    ContainsNul {
        /// Kind tag.
        kind: &'static str,
    },
}

/// Maximum byte length for a `host_dev_name`. Mirrors the API-layer cap that's been
/// in force since Phase 2 (see `crates/api/src/schemas/network.rs`).
pub const HOST_DEV_NAME_MAX_BYTES: usize = 64;

/// Validated host-side network device name (the `host_dev_name` field on a
/// `/network-interfaces/{id}` PUT body).
///
/// Validation: non-empty, ≤ 64 bytes, no NUL byte. Charset is left intentionally open
/// because the value is opaque on macOS — squib derives a vmnet handle name from
/// `iface_id`, not from `host_dev_name`, so we only need the length / NUL guard here.
///
/// The newtype is `Serialize` + `Deserialize`-transparent so it round-trips through
/// snapshot save/restore (see I-NET-3) without an envelope-format change.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub struct HostDevName(String);

impl HostDevName {
    /// Wrap a host device name after running the boundary checks.
    ///
    /// # Errors
    /// Surfaces [`IdentifierError`] on empty / oversize / NUL-bearing input.
    pub fn new(name: impl Into<String>) -> Result<Self, IdentifierError> {
        let name = name.into();
        if name.is_empty() {
            return Err(IdentifierError::Empty {
                kind: "host_dev_name",
            });
        }
        if name.len() > HOST_DEV_NAME_MAX_BYTES {
            return Err(IdentifierError::TooLong {
                kind: "host_dev_name",
                len: name.len(),
                max: HOST_DEV_NAME_MAX_BYTES,
            });
        }
        if name.as_bytes().contains(&0) {
            return Err(IdentifierError::ContainsNul {
                kind: "host_dev_name",
            });
        }
        Ok(Self(name))
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the newtype and yield the inner string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for HostDevName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for HostDevName {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_short_ascii_name() {
        let n = HostDevName::new("vmnet0").unwrap();
        assert_eq!(n.as_str(), "vmnet0");
    }

    #[test]
    fn test_should_reject_empty() {
        let err = HostDevName::new("").unwrap_err();
        assert!(matches!(err, IdentifierError::Empty { .. }));
    }

    #[test]
    fn test_should_reject_overlong() {
        let huge = "a".repeat(HOST_DEV_NAME_MAX_BYTES + 1);
        let err = HostDevName::new(huge).unwrap_err();
        assert!(matches!(err, IdentifierError::TooLong { .. }));
    }

    #[test]
    fn test_should_reject_nul_byte() {
        let err = HostDevName::new("vmnet\0bad").unwrap_err();
        assert!(matches!(err, IdentifierError::ContainsNul { .. }));
    }

    #[test]
    fn test_should_round_trip_through_serde_json() {
        let n = HostDevName::new("eth0").unwrap();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"eth0\"");
        let back: HostDevName = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), "eth0");
    }

    #[test]
    fn test_should_reject_invalid_value_through_deserialize() {
        let json = "\"\""; // empty string
        let err = serde_json::from_str::<HostDevName>(json).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
