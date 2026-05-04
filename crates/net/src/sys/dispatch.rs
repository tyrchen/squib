//! Minimal `libdispatch` wrappers used by the vmnet binding.
//!
//! Only `dispatch_queue_create` and `dispatch_semaphore_*` are exercised here —
//! enough to wait on `vmnet_start_interface` / `vmnet_stop_interface` callbacks
//! and to provide the per-interface serial dispatch queue vmnet's read/write
//! callbacks land on.

//! Note: the `dispatch_*` extern signatures use `dispatch_object_t`
//! transparent types which are pointer-sized; we model them as `*mut c_void`
//! and let the libdispatch retain count handle ABI correctness.

use std::{
    ffi::CString,
    io,
    os::raw::{c_char, c_long, c_void},
    ptr,
};

#[link(name = "System", kind = "framework")]
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
    fn dispatch_semaphore_create(value: c_long) -> *mut c_void;
    fn dispatch_semaphore_signal(dsema: *mut c_void) -> c_long;
    fn dispatch_semaphore_wait(dsema: *mut c_void, timeout: u64) -> c_long;
    fn dispatch_time(when: u64, delta: i64) -> u64;
}

/// `DISPATCH_TIME_FOREVER` from `<dispatch/time.h>`.
pub const DISPATCH_TIME_FOREVER: u64 = u64::MAX;

/// `DISPATCH_TIME_NOW` from `<dispatch/time.h>`.
const DISPATCH_TIME_NOW: u64 = 0;

/// Build a "now + nanoseconds" `dispatch_time_t` by calling libdispatch's
/// own `dispatch_time` helper. We **must** go through `dispatch_time`: a
/// raw nanosecond count is not interpretable as a `dispatch_time_t` (which
/// lives in libdispatch's own clock domain — typically `mach_absolute_time`-
/// based on Apple Silicon). Passing a raw nanosecond value to
/// `dispatch_semaphore_wait` makes it interpret the value as an already-
/// past absolute timestamp and the wait returns immediately.
#[must_use]
pub fn dispatch_time_now_plus_ns(delta_ns: u64) -> u64 {
    let delta = i64::try_from(delta_ns).unwrap_or(i64::MAX);
    // SAFETY: `dispatch_time` accepts a `dispatch_time_t when` (0 = NOW)
    // and a signed nanosecond delta. The call is reentrant and has no
    // pointer arguments.
    unsafe { dispatch_time(DISPATCH_TIME_NOW, delta) }
}

/// A serial `dispatch_queue_t`. RAII; calls `dispatch_release` on drop.
#[derive(Debug)]
pub struct Queue {
    raw: *mut c_void,
}

// SAFETY: `dispatch_queue_t` is reference-counted and thread-safe. `dispatch_release`
// is balanced against the `dispatch_queue_create` retain.
unsafe impl Send for Queue {}
unsafe impl Sync for Queue {}

impl Queue {
    /// Create a serial queue with the given label. Returns an error if
    /// libdispatch cannot allocate the queue (out-of-memory) or if the label
    /// contains an interior NUL byte. CLAUDE.md § Safety forbids panicking
    /// here: this is a boundary call invoked once per VM start.
    ///
    /// # Errors
    /// `io::ErrorKind::InvalidInput` if `label` contains a NUL;
    /// `io::ErrorKind::OutOfMemory` if `dispatch_queue_create` returns NULL.
    pub fn create_serial(label: &str) -> Result<Self, io::Error> {
        let cstr = CString::new(label).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("dispatch queue label contains NUL: {e}"),
            )
        })?;
        // SAFETY: `dispatch_queue_create` accepts a NUL-terminated UTF-8 label
        // and a `dispatch_queue_attr_t` (or NULL for serial). `cstr` lives
        // until end of statement; vmnet only retains the queue, not the label.
        let raw = unsafe { dispatch_queue_create(cstr.as_ptr(), ptr::null_mut()) };
        if raw.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "dispatch_queue_create returned NULL",
            ));
        }
        Ok(Self { raw })
    }

    /// Raw pointer for FFI calls. Borrowed; the queue's lifetime is tied to `&self`.
    #[must_use]
    pub fn as_ptr(&self) -> *mut c_void {
        self.raw
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: `dispatch_release` decrements the retain count; we held
            // the strong reference from `dispatch_queue_create`.
            unsafe { dispatch_release(self.raw) };
        }
    }
}

/// `dispatch_semaphore_t`. RAII; calls `dispatch_release` on drop.
#[derive(Debug)]
pub struct Semaphore {
    raw: *mut c_void,
}

// SAFETY: `dispatch_semaphore_t` is reference-counted and thread-safe.
unsafe impl Send for Semaphore {}
unsafe impl Sync for Semaphore {}

impl Semaphore {
    /// Build a semaphore with initial value `0` — the standard "wait for one
    /// callback" pattern: caller waits, callback signals.
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: `dispatch_semaphore_create(0)` returns a fresh semaphore
        // with retain count 1; we own it until Drop.
        let raw = unsafe { dispatch_semaphore_create(0) };
        Self { raw }
    }

    /// Wait for a signal, with the given timeout. Returns `true` if signalled,
    /// `false` if timed out. `DISPATCH_TIME_FOREVER` blocks indefinitely.
    pub fn wait(&self, timeout: u64) -> bool {
        // SAFETY: `dispatch_semaphore_wait` accepts a non-NULL semaphore and a
        // `dispatch_time_t`. Returns 0 on success, non-zero on timeout.
        unsafe { dispatch_semaphore_wait(self.raw, timeout) == 0 }
    }

    /// Signal the semaphore. Returns `true` if a thread was woken.
    pub fn signal(&self) -> bool {
        // SAFETY: `dispatch_semaphore_signal` accepts a non-NULL semaphore.
        unsafe { dispatch_semaphore_signal(self.raw) != 0 }
    }
}

impl Default for Semaphore {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: balances the retain from `dispatch_semaphore_create`.
            unsafe { dispatch_release(self.raw) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_signal_and_wait_a_dispatch_semaphore() {
        let sem = Semaphore::new();
        // Signal returns true if it woke a waiter; with no waiter yet it
        // returns false. Either is fine — what matters is the semaphore
        // round-trip below.
        let _ = sem.signal();
        assert!(sem.wait(DISPATCH_TIME_FOREVER));
    }

    #[test]
    fn test_should_create_and_drop_a_serial_queue() {
        let q = Queue::create_serial("squib-net.tests").expect("queue creation");
        assert!(!q.as_ptr().is_null());
    }

    #[test]
    fn test_should_reject_label_with_nul_byte() {
        let err = Queue::create_serial("bad\0label").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
