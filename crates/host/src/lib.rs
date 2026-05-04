//! `squib-host` — macOS host-side primitives that back the snapshot subsystem.
//!
//! Phase 5.5 lands the Mach-exception-port pager that powers `--mem-backend=Uffd`
//! and `--mem-backend=File` postcopy restore (per
//! [16-snapshots.md § 5](../../../specs/16-snapshots.md#5-postcopy--lazy-restore)).
//! The crate is host-specific: on macOS it wires raw Mach syscalls; elsewhere it
//! exposes the same trait surface with stub backends so the snapshot crate's tests
//! still build cross-platform.
//!
//! The crate is the `unsafe` boundary alongside `squib-hv` and `squib-net::sys`.

#![warn(missing_docs)]
// Hardware names (Mach, IPC, mig, EXC_*) and hyphenated nouns are common in the
// docs; backticking each adds noise.
#![allow(clippy::doc_markdown)]
// Boundary modules call into raw FFI; the unsafe-op-in-unsafe-fn lint is what
// enforces the SAFETY: blocks per CLAUDE.md.
#![cfg_attr(not(target_os = "macos"), forbid(unsafe_code))]
#![cfg_attr(target_os = "macos", deny(unsafe_op_in_unsafe_fn))]
// Sync I/O in tests is fine; the production pager runs on a dedicated `std::thread`
// and synchronously serves Mach exceptions. The project's `disallowed_methods` lint
// targets runtime tokio code (per `crates/vmm/src/lib.rs`).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
// `as usize` for the host page size is safe — Apple Silicon is 64-bit. The static
// guarantee lives in `host_page_size`; truncation is impossible.
#![allow(clippy::cast_possible_truncation)]

pub mod pager;

pub use pager::{
    FilePageSource, PageRequest, PageSource, PageSourceError, Pager, PagerConfig, PagerError,
    PagerStats, PagerStatsSnapshot, PrewarmList, UffdPageSource, host_page_size, spawn_mach_server,
};
