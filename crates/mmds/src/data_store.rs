//! MMDS data store — JSON tree + JSON Pointer traversal + size cap.
//!
//! Per [15-mmds.md § 4](../../../specs/15-mmds.md#4-api-surface):
//!
//! - `PUT /mmds` replaces the JSON tree.
//! - `PATCH /mmds` applies an RFC 7396 merge-patch.
//! - `GET /mmds` returns the tree.
//! - `GET /mmds/<json-pointer>` returns the subtree at the pointer.
//!
//! Per [15-mmds.md § 5](../../../specs/15-mmds.md#5-behaviour-edges):
//!
//! - `--mmds-size-limit <bytes>` (default 51200) caps the JSON tree.
//! - `PUT /mmds` exceeding the cap returns 413.
//! - The store is never logged at `info` or below (I-MMDS-4); callers are responsible for not
//!   stamping the JSON into a `tracing::info!`.

use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::Value;
use thiserror::Error;

/// MMDS protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MmdsVersion {
    /// V1 — no token required.
    #[default]
    V1,
    /// V2 — every read requires a token issued by the V2 token endpoint.
    V2,
}

/// Errors produced by [`Mmds`] operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MmdsError {
    /// `PUT` body exceeds the configured size cap.
    #[error("MMDS body exceeds size cap of {cap} bytes (got {got})")]
    SizeLimitExceeded {
        /// Configured size cap.
        cap: usize,
        /// Actual body size.
        got: usize,
    },

    /// JSON Pointer query targeted a path that is not present.
    #[error("MMDS path not found: {0}")]
    PathNotFound(String),

    /// JSON Pointer is syntactically invalid.
    #[error("invalid JSON pointer: {0}")]
    InvalidPointer(String),

    /// Body is not valid JSON.
    #[error("invalid JSON body: {0}")]
    InvalidJson(String),
}

/// Microvm Metadata Service — JSON tree + V2 token store.
///
/// Cloning is cheap and intentional: an `Arc<RwLock<Value>>` is shared
/// between the API server (writers) and the MMDS interceptor (readers).
#[derive(Debug, Clone)]
pub struct Mmds {
    inner: Arc<MmdsInner>,
}

#[derive(Debug)]
struct MmdsInner {
    version: RwLock<MmdsVersion>,
    /// Tree contents protected by an `RwLock` — many readers, rare writers.
    data: RwLock<Value>,
    /// Hard size cap for the serialized tree. Writes that would exceed it
    /// fail with `SizeLimitExceeded`.
    size_cap: usize,
}

impl Mmds {
    /// Build an empty MMDS with the given size cap (default 51200).
    #[must_use]
    pub fn new(size_cap: usize) -> Self {
        Self {
            inner: Arc::new(MmdsInner {
                version: RwLock::new(MmdsVersion::default()),
                data: RwLock::new(Value::Object(serde_json::Map::new())),
                size_cap,
            }),
        }
    }

    /// Currently configured MMDS version.
    #[must_use]
    pub fn version(&self) -> MmdsVersion {
        *self.inner.version.read()
    }

    /// Switch to V1 / V2.
    pub fn set_version(&self, version: MmdsVersion) {
        *self.inner.version.write() = version;
    }

    /// Configured size cap.
    #[must_use]
    pub fn size_cap(&self) -> usize {
        self.inner.size_cap
    }

    /// Replace the tree with `body`. Rejects if the serialized form exceeds
    /// the cap.
    ///
    /// # Errors
    /// - [`MmdsError::SizeLimitExceeded`] if the serialized body is over the cap.
    /// - [`MmdsError::InvalidJson`] if `body` is not parseable.
    pub fn put_json(&self, body: &str) -> Result<(), MmdsError> {
        if body.len() > self.inner.size_cap {
            return Err(MmdsError::SizeLimitExceeded {
                cap: self.inner.size_cap,
                got: body.len(),
            });
        }
        let value: Value =
            serde_json::from_str(body).map_err(|e| MmdsError::InvalidJson(e.to_string()))?;
        let mut guard = self.inner.data.write();
        *guard = value;
        Ok(())
    }

    /// Apply an RFC 7396 merge patch.
    ///
    /// # Errors
    /// - [`MmdsError::SizeLimitExceeded`] if the post-patch tree exceeds the cap.
    /// - [`MmdsError::InvalidJson`] if `body` is not parseable.
    pub fn patch_json(&self, body: &str) -> Result<(), MmdsError> {
        let patch: Value =
            serde_json::from_str(body).map_err(|e| MmdsError::InvalidJson(e.to_string()))?;
        let mut guard = self.inner.data.write();
        let mut merged = guard.clone();
        merge_patch(&mut merged, &patch);
        let serialized = merged.to_string();
        if serialized.len() > self.inner.size_cap {
            return Err(MmdsError::SizeLimitExceeded {
                cap: self.inner.size_cap,
                got: serialized.len(),
            });
        }
        *guard = merged;
        Ok(())
    }

    /// Return the entire tree.
    #[must_use]
    pub fn get_root(&self) -> Value {
        self.inner.data.read().clone()
    }

    /// Look up the subtree at the given JSON Pointer (e.g. `/foo/0/bar`).
    ///
    /// # Errors
    /// - [`MmdsError::PathNotFound`] if the pointer does not resolve.
    pub fn get_at_pointer(&self, pointer: &str) -> Result<Value, MmdsError> {
        let guard = self.inner.data.read();
        if pointer.is_empty() || pointer == "/" {
            return Ok(guard.clone());
        }
        match guard.pointer(pointer) {
            Some(v) => Ok(v.clone()),
            None => Err(MmdsError::PathNotFound(pointer.to_string())),
        }
    }
}

/// RFC 7396 merge-patch implementation. `null` in the patch removes the key;
/// objects recurse, scalars replace.
fn merge_patch(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(t), Value::Object(p)) => {
            for (k, pv) in p {
                if pv.is_null() {
                    t.remove(k);
                } else {
                    let entry = t.entry(k.clone()).or_insert(Value::Null);
                    merge_patch(entry, pv);
                }
            }
        }
        (target, patch) => {
            *target = patch.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_default_to_v1() {
        let m = Mmds::new(1024);
        assert_eq!(m.version(), MmdsVersion::V1);
    }

    #[test]
    fn test_should_round_trip_put_and_get_root() {
        let m = Mmds::new(1024);
        m.put_json(r#"{"latest":{"meta-data":{"instance-id":"i-1234"}}}"#)
            .unwrap();
        let v = m.get_root();
        assert_eq!(v["latest"]["meta-data"]["instance-id"], "i-1234");
    }

    #[test]
    fn test_should_serve_subtree_at_json_pointer() {
        let m = Mmds::new(1024);
        m.put_json(r#"{"a":{"b":[10,20,30]}}"#).unwrap();
        let v = m.get_at_pointer("/a/b/1").unwrap();
        assert_eq!(v, serde_json::json!(20));
    }

    #[test]
    fn test_should_reject_put_over_size_cap() {
        let m = Mmds::new(8);
        let err = m.put_json(r#"{"key":"too-large"}"#).unwrap_err();
        assert!(matches!(err, MmdsError::SizeLimitExceeded { .. }));
    }

    #[test]
    fn test_should_reject_invalid_json_body() {
        let m = Mmds::new(1024);
        let err = m.put_json("not json").unwrap_err();
        assert!(matches!(err, MmdsError::InvalidJson(_)));
    }

    #[test]
    fn test_should_apply_rfc_7396_merge_patch() {
        let m = Mmds::new(1024);
        m.put_json(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap();
        m.patch_json(r#"{"b":{"c":20,"e":4},"f":5}"#).unwrap();
        let root = m.get_root();
        assert_eq!(root["a"], 1);
        assert_eq!(root["b"]["c"], 20);
        assert_eq!(root["b"]["d"], 3);
        assert_eq!(root["b"]["e"], 4);
        assert_eq!(root["f"], 5);
    }

    #[test]
    fn test_should_remove_key_with_null_in_patch_per_rfc_7396() {
        let m = Mmds::new(1024);
        m.put_json(r#"{"a":1,"b":2}"#).unwrap();
        m.patch_json(r#"{"a":null}"#).unwrap();
        let root = m.get_root();
        assert!(root.get("a").is_none());
        assert_eq!(root["b"], 2);
    }

    #[test]
    fn test_should_404_on_unknown_pointer() {
        let m = Mmds::new(1024);
        m.put_json(r#"{"a":1}"#).unwrap();
        let err = m.get_at_pointer("/missing").unwrap_err();
        assert!(matches!(err, MmdsError::PathNotFound(_)));
    }

    #[test]
    fn test_should_share_underlying_store_across_clones() {
        let a = Mmds::new(1024);
        let b = a.clone();
        a.put_json(r#"{"x":42}"#).unwrap();
        assert_eq!(b.get_root()["x"], 42);
    }
}
