//! Minimal XPC bindings for `vmnet` parameter dictionaries.
//!
//! `vmnet_start_interface` consumes an `xpc_object_t` dictionary and yields
//! one (containing the negotiated MTU, MAC, max packet size). We only bind:
//!
//! - `xpc_dictionary_create` — empty dictionary
//! - `xpc_dictionary_set_string` / `_uint64`
//! - `xpc_dictionary_get_string` / `_uint64` / `_data`
//! - `xpc_release` / `xpc_retain`
//!
//! plus a thin RAII wrapper that calls `xpc_release` on drop.

use std::{
    ffi::{CStr, CString},
    os::raw::{c_char, c_void},
    ptr, slice,
};

#[link(name = "System", kind = "framework")]
unsafe extern "C" {
    fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const *const c_void,
        count: usize,
    ) -> *mut c_void;
    fn xpc_dictionary_set_string(xdict: *mut c_void, key: *const c_char, value: *const c_char);
    fn xpc_dictionary_set_uint64(xdict: *mut c_void, key: *const c_char, value: u64);
    fn xpc_dictionary_set_bool(xdict: *mut c_void, key: *const c_char, value: bool);
    /// `xpc_dictionary_set_uuid` — write a 16-byte `uuid_t` under the key. The
    /// `uuid` parameter must point to exactly 16 bytes.
    fn xpc_dictionary_set_uuid(xdict: *mut c_void, key: *const c_char, uuid: *const u8);
    fn xpc_dictionary_get_string(xdict: *mut c_void, key: *const c_char) -> *const c_char;
    fn xpc_dictionary_get_uint64(xdict: *mut c_void, key: *const c_char) -> u64;
    fn xpc_dictionary_get_data(
        xdict: *mut c_void,
        key: *const c_char,
        length: *mut usize,
    ) -> *const c_void;
    fn xpc_release(xpc_object: *mut c_void);
    fn xpc_retain(xpc_object: *mut c_void) -> *mut c_void;
}

/// RAII wrapper for an `xpc_object_t`. Drop calls `xpc_release`.
#[derive(Debug)]
pub struct XpcObject {
    raw: *mut c_void,
}

// SAFETY: XPC objects are reference-counted and thread-safe; `xpc_release`
// from any thread is supported.
unsafe impl Send for XpcObject {}
unsafe impl Sync for XpcObject {}

impl XpcObject {
    /// Create an empty XPC dictionary.
    #[must_use]
    pub fn new_dictionary() -> Self {
        // SAFETY: passing (NULL, NULL, 0) creates an empty mutable dictionary
        // per `<xpc/xpc.h>`. The returned object has retain count 1; we own it.
        let raw = unsafe { xpc_dictionary_create(ptr::null(), ptr::null(), 0) };
        Self { raw }
    }

    /// Wrap an existing `xpc_object_t` for which the caller already holds a
    /// strong reference (i.e. vmnet retained the param before invoking the
    /// callback). NULL is preserved as `None`.
    ///
    /// # Safety
    /// `raw` must be a strong-counted `xpc_object_t`. The wrapper takes ownership
    /// of that count.
    pub unsafe fn from_raw_retained_or_null(raw: *mut c_void) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self { raw })
        }
    }

    /// Borrow the raw pointer for FFI calls.
    #[must_use]
    pub fn as_ptr(&self) -> *mut c_void {
        self.raw
    }

    /// Retain (clone the strong count). Returns a new RAII wrapper.
    #[must_use]
    pub fn retain(&self) -> Self {
        // SAFETY: `xpc_retain` increments the retain count; the resulting raw
        // pointer is owned by the new wrapper.
        let raw = unsafe { xpc_retain(self.raw) };
        Self { raw }
    }

    /// Set a string value under `key`.
    ///
    /// Both `key` and `value` must be free of interior NUL bytes; on failure
    /// the call is a no-op and a warning is logged. Vmnet keys are static
    /// strings (see [`super::vmnet::keys`]) so this branch is unreachable in
    /// production; the warning catches a programmer error rather than a
    /// silent dictionary corruption.
    pub fn set_string(&self, key: &CStr, value: &str) {
        let Ok(val_c) = CString::new(value) else {
            tracing::warn!(?key, "xpc set_string: value contained NUL; skipping");
            return;
        };
        // SAFETY: both CStrings live for the duration of the call. `xpc_dictionary_*`
        // copies the bytes; no lifetime escape.
        unsafe { xpc_dictionary_set_string(self.raw, key.as_ptr(), val_c.as_ptr()) };
    }

    /// Set a `u64` value under `key`.
    pub fn set_uint64(&self, key: &CStr, value: u64) {
        // SAFETY: see `set_string`.
        unsafe { xpc_dictionary_set_uint64(self.raw, key.as_ptr(), value) };
    }

    /// Set a `bool` value under `key`.
    pub fn set_bool(&self, key: &CStr, value: bool) {
        // SAFETY: see `set_string`.
        unsafe { xpc_dictionary_set_bool(self.raw, key.as_ptr(), value) };
    }

    /// Set a `uuid_t` (16 raw bytes) value under `key`. Vmnet's
    /// `vmnet_interface_id_key` expects this shape, not a hex-string.
    pub fn set_uuid(&self, key: &CStr, uuid: &[u8; 16]) {
        // SAFETY: `xpc_dictionary_set_uuid` reads exactly 16 bytes from `uuid`.
        // The `&[u8; 16]` borrow guarantees the length and lifetime; the call
        // copies the bytes synchronously into the dictionary.
        unsafe { xpc_dictionary_set_uuid(self.raw, key.as_ptr(), uuid.as_ptr()) };
    }

    /// Read a `u64` value under `key` (returns `0` if absent or wrong type).
    #[must_use]
    pub fn get_uint64(&self, key: &CStr) -> u64 {
        // SAFETY: `xpc_dictionary_get_uint64` returns 0 for missing / wrong-type
        // keys and is safe to call on any retained xpc_object_t.
        unsafe { xpc_dictionary_get_uint64(self.raw, key.as_ptr()) }
    }

    /// Read a string value under `key`. Returns `None` if absent or wrong type.
    #[must_use]
    pub fn get_string(&self, key: &CStr) -> Option<String> {
        // SAFETY: `xpc_dictionary_get_string` returns either NULL or a borrowed
        // pointer valid for the lifetime of `self`. We copy into an owned String
        // immediately so no aliasing escapes.
        let raw = unsafe { xpc_dictionary_get_string(self.raw, key.as_ptr()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: vmnet emits NUL-terminated UTF-8 for every documented string
        // key; the borrowed pointer is valid for as long as `self` lives.
        let bytes = unsafe { CStr::from_ptr(raw) }.to_bytes();
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Read a binary blob under `key`. Returns `None` if absent or wrong type.
    #[must_use]
    pub fn get_data(&self, key: &CStr) -> Option<Vec<u8>> {
        let mut len: usize = 0;
        // SAFETY: `xpc_dictionary_get_data` returns either NULL or a borrowed
        // pointer + length valid for the lifetime of `self`. We immediately
        // copy into an owned Vec so no borrow escapes.
        let raw = unsafe { xpc_dictionary_get_data(self.raw, key.as_ptr(), &raw mut len) };
        if raw.is_null() || len == 0 {
            return None;
        }
        // SAFETY: vmnet documents `xpc_dictionary_get_data` as returning a
        // pointer to `len` bytes; we copy them into an owned Vec.
        let slice = unsafe { slice::from_raw_parts(raw.cast::<u8>(), len) };
        Some(slice.to_vec())
    }
}

impl Drop for XpcObject {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: balances the retain count owned by `self`.
            unsafe { xpc_release(self.raw) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_round_trip_uint64_through_xpc_dictionary() {
        let dict = XpcObject::new_dictionary();
        dict.set_uint64(c"vmnet_operation_mode_key", 1000);
        assert_eq!(dict.get_uint64(c"vmnet_operation_mode_key"), 1000);
    }

    #[test]
    fn test_should_round_trip_string_through_xpc_dictionary() {
        let dict = XpcObject::new_dictionary();
        dict.set_string(
            c"vmnet_interface_id_key",
            "00112233-4455-6677-8899-AABBCCDDEEFF",
        );
        assert_eq!(
            dict.get_string(c"vmnet_interface_id_key").as_deref(),
            Some("00112233-4455-6677-8899-AABBCCDDEEFF")
        );
    }

    #[test]
    fn test_should_return_none_for_missing_string_key() {
        let dict = XpcObject::new_dictionary();
        assert!(dict.get_string(c"missing").is_none());
    }

    #[test]
    fn test_should_skip_set_string_when_value_contains_nul() {
        let dict = XpcObject::new_dictionary();
        dict.set_string(c"k", "with\0nul");
        // The set was a no-op; getting the key returns None.
        assert!(dict.get_string(c"k").is_none());
    }
}
