//! Error type for `squib-jail`. Variants map 1:1 to operator-visible messages
//! whose exit code is asserted by the I-JAIL-2 test suite (see
//! `specs/40-jailer.md` § 5). Renaming a variant is a compat-suite golden
//! change, the same as upstream's `JailerError`.

use std::{io, path::PathBuf};

/// Errors surfaced by `squib-jail`. Each variant carries enough context for an
/// operator to triage the failure without re-running the binary under `strace`
/// (we have no `strace` on Darwin anyway).
#[derive(Debug, thiserror::Error)]
pub(crate) enum JailerError {
    /// `--id` failed the `^[A-Za-z0-9_-]{1,64}$` charset / length check from
    /// `specs/40-jailer.md` § 2.1 (matches upstream jailer's `MAX_ID_LENGTH`
    /// = 64 + the upstream allowlist that includes hyphens — launchers
    /// like `firecracker-go-sdk` generate hyphenated ids).
    #[error("invalid instance id: {0}")]
    InvalidInstanceId(String),

    /// `--resource-limit <key>=<value>` parse failure.
    #[error("invalid --resource-limit value: {0}")]
    InvalidResourceLimit(String),

    /// `--resource-limit <key>` is not one of the documented set.
    #[error("unsupported resource: {0}; expected `fsize` or `no-file`")]
    UnsupportedResource(String),

    /// `sandbox_init(3)` returned non-zero. The internal pointer carries the
    /// human-readable error from libsandbox; we surface it verbatim per
    /// I-JAIL-4.
    #[error("sandbox_init failed: {0}")]
    SandboxInit(String),

    /// `chroot(2)` failed.
    #[error("failed to chroot to {path}: {source}")]
    Chroot {
        /// The directory we tried to chroot into.
        path: PathBuf,
        /// The libc-level cause.
        #[source]
        source: io::Error,
    },

    /// `chdir(2)` to `/` after chroot failed.
    #[error("failed to chdir to / after chroot: {0}")]
    ChdirRoot(#[source] io::Error),

    /// `setgid(2)` failed.
    #[error("failed to setgid to {gid}: {source}")]
    Setgid {
        /// Target gid.
        gid: libc::gid_t,
        /// The libc-level cause.
        #[source]
        source: io::Error,
    },

    /// `setuid(2)` failed.
    #[error("failed to setuid to {uid}: {source}")]
    Setuid {
        /// Target uid.
        uid: libc::uid_t,
        /// The libc-level cause.
        #[source]
        source: io::Error,
    },

    /// `setrlimit(2)` failed.
    #[error("failed to set rlimit for {key}: {source}")]
    Setrlimit {
        /// The resource key (`fsize` / `no-file`).
        key: &'static str,
        /// The libc-level cause.
        #[source]
        source: io::Error,
    },

    /// `setsid(2)` failed during `--daemonize`.
    #[error("failed to setsid: {0}")]
    Setsid(#[source] io::Error),

    /// `dup2(2)` of `/dev/null` onto stdio failed during `--daemonize`.
    #[error("failed to redirect stdio onto /dev/null: {0}")]
    RedirectStdio(#[source] io::Error),

    /// Could not open `/dev/null` for the daemonize fd-redirect step.
    #[error("failed to open /dev/null: {0}")]
    OpenDevNull(#[source] io::Error),

    /// Could not create the chroot directory tree.
    #[error("failed to create chroot directory {path}: {source}")]
    CreateChroot {
        /// The chroot path.
        path: PathBuf,
        /// The libc-level cause.
        #[source]
        source: io::Error,
    },

    /// Could not stage `--exec-file` inside the chroot.
    #[error("failed to copy {src} → {dst}: {source}")]
    StageExecFile {
        /// Source path (host).
        src: PathBuf,
        /// Destination path (inside chroot).
        dst: PathBuf,
        /// The libc-level cause.
        #[source]
        source: io::Error,
    },

    /// `--exec-file` was missing or not a regular file.
    #[error("--exec-file path is not a regular file: {0}")]
    ExecFileNotRegular(PathBuf),

    /// `--exec-file` could not be canonicalized.
    #[error("failed to canonicalize --exec-file {path}: {source}")]
    Canonicalize {
        /// The path that failed to canonicalize.
        path: PathBuf,
        /// The libc-level cause.
        #[source]
        source: io::Error,
    },

    /// Path contains an interior NUL byte and cannot be passed to a libc
    /// syscall.
    #[error("path contains an interior NUL byte: {0}")]
    PathContainsNul(PathBuf),

    /// `execv(2)` returned (it should not — on success `execv` does not return).
    #[error("failed to exec into the staged binary {0}: {1}")]
    Exec(PathBuf, #[source] io::Error),
}

impl JailerError {
    /// The exit code surfaced to the shell. Mirrors the upstream `jailer`
    /// convention of "1 for any failure"; the I-JAIL-2 invariant only asks
    /// that the exit code be non-zero and stable per error class. We keep
    /// the door open for fine-grained codes per variant if compat
    /// regressions show up later.
    pub(crate) fn exit_code(&self) -> i32 {
        match self {
            // Argument-parse / validation failures — exit 1.
            Self::InvalidInstanceId(_)
            | Self::InvalidResourceLimit(_)
            | Self::UnsupportedResource(_)
            | Self::PathContainsNul(_)
            | Self::ExecFileNotRegular(_)
            | Self::Canonicalize { .. } => 1,
            // Privilege / sandbox failures — exit 2.
            Self::Chroot { .. }
            | Self::ChdirRoot(_)
            | Self::Setgid { .. }
            | Self::Setuid { .. }
            | Self::Setrlimit { .. }
            | Self::Setsid(_)
            | Self::RedirectStdio(_)
            | Self::OpenDevNull(_)
            | Self::SandboxInit(_)
            | Self::CreateChroot { .. }
            | Self::StageExecFile { .. } => 2,
            // execv failures — exit 3 (the staged binary is in place but
            // could not start; this is the same shape as upstream's
            // `JailerError::Exec`).
            Self::Exec(_, _) => 3,
        }
    }
}

/// Crate-local result type.
pub(crate) type Result<T> = std::result::Result<T, JailerError>;
