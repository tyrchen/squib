//! macOS `sandbox_init(3)` wrapper for `--macos-sandbox-profile`.
//!
//! The bundled `.sb` profiles are embedded at compile time (see
//! `apps/squib-jail/profiles/`); we hand the matching profile body to the
//! libsandbox FFI just before `execv(2)`.
//!
//! `sandbox_init(3)` is the *only* libc call in this crate that has no safe
//! wrapper in either `nix` or `libc`. The rest of the privilege-drop sequence
//! goes through libc constants and integer-return syscalls; this one returns a
//! malloc'd error buffer through an out-pointer. We isolate the FFI here
//! exactly like `squib-net::sys` isolates `vmnet.framework`.

use crate::{
    cli::SandboxProfile,
    error::{JailerError, Result},
};

/// `default` profile body. Deny network egress, allow vmnet socket, allow
/// `/srv/jailer/<id>` r/w.
const PROFILE_DEFAULT: &str = include_str!("../profiles/default.sb");

/// `permissive` profile body. Close to no sandboxing; for debugging.
const PROFILE_PERMISSIVE: &str = include_str!("../profiles/permissive.sb");

/// Resolve a [`SandboxProfile`] enum variant to the embedded profile body.
pub(crate) fn profile_body(profile: SandboxProfile) -> &'static str {
    match profile {
        SandboxProfile::Default => PROFILE_DEFAULT,
        SandboxProfile::Permissive => PROFILE_PERMISSIVE,
    }
}

/// Apply a `sandbox_init(3)` profile in-process. On success the calling
/// process is sandboxed; the call is one-shot per process and inherits across
/// `execv(2)` (per the libsandbox contract).
///
/// Returns [`JailerError::SandboxInit`] with the libsandbox error string when
/// the call returns non-zero (see I-JAIL-4 in `specs/40-jailer.md` § 5).
#[cfg(target_os = "macos")]
pub(crate) fn apply_profile(profile: SandboxProfile) -> Result<()> {
    use std::ffi::{CStr, CString};

    let body = profile_body(profile);
    let body_c = CString::new(body)
        .map_err(|_| JailerError::SandboxInit("profile body contains a NUL byte".into()))?;
    let mut errbuf: *mut libc::c_char = std::ptr::null_mut();

    // SAFETY: `sandbox_init` reads the NUL-terminated `body_c` for the
    // duration of the call, and writes a pointer to a malloc'd error buffer
    // through `errbuf` only on failure. We free that buffer below via
    // `sandbox_free_error`. `flags = 0` means "interpret `profile` as a raw
    // SBPL body" per the libsandbox header.
    let rc = unsafe { ffi::sandbox_init(body_c.as_ptr(), 0, &raw mut errbuf) };
    if rc == 0 {
        Ok(())
    } else {
        // SAFETY: when `sandbox_init` returns non-zero it sets `errbuf` to a
        // malloc'd, NUL-terminated C string. We copy it into an owned
        // `String` before handing it back to libsandbox via
        // `sandbox_free_error` so the buffer isn't leaked.
        let msg = if errbuf.is_null() {
            "unknown sandbox_init error".to_string()
        } else {
            let s = unsafe { CStr::from_ptr(errbuf) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: `errbuf` is the libsandbox-owned malloc buffer.
            unsafe { ffi::sandbox_free_error(errbuf) };
            s
        };
        Err(JailerError::SandboxInit(msg))
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn apply_profile(_profile: SandboxProfile) -> Result<()> {
    Err(JailerError::SandboxInit(
        "sandbox_init is only available on macOS".into(),
    ))
}

#[cfg(target_os = "macos")]
mod ffi {
    use libc::{c_char, c_int, c_uint};

    // The C symbols come from libSystem (System framework) on macOS. The
    // header is `<sandbox.h>`. The Rust binding is intentionally minimal
    // and lives entirely inside this private module; nothing outside
    // `sandbox.rs` ever sees a raw FFI pointer (`I-NET-1`-style boundary).
    unsafe extern "C" {
        pub(super) fn sandbox_init(
            profile: *const c_char,
            flags: c_uint,
            errorbuf: *mut *mut c_char,
        ) -> c_int;

        pub(super) fn sandbox_free_error(errorbuf: *mut c_char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_body_default_is_non_empty() {
        let body = profile_body(SandboxProfile::Default);
        assert!(
            body.starts_with("(version 1)"),
            "default profile must be SBPL: {body}"
        );
    }

    #[test]
    fn profile_body_permissive_is_non_empty() {
        let body = profile_body(SandboxProfile::Permissive);
        assert!(
            body.starts_with("(version 1)"),
            "permissive profile must be SBPL: {body}"
        );
    }

    /// Apply the permissive profile end-to-end on macOS. This is a real
    /// `sandbox_init` call against the live framework, so it's gated to
    /// macOS only and still safe (permissive ≈ no-op).
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "modifies the process sandbox state; opt-in via `cargo test -- --include-ignored`"]
    fn apply_permissive_profile_succeeds() {
        apply_profile(SandboxProfile::Permissive).expect("permissive profile must apply");
    }
}
