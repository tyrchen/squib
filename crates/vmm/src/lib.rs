//! Squib VMM core.
//!
//! Wires together the foundation crates from Phase 1 (squib-arch, squib-loader,
//! squib-fdt, squib-hv, squib-gic) into a single boot orchestration entry point:
//! [`builder::build_microvm_for_boot`]. Phase 1.6 ships the orchestration scaffolding;
//! later phases attach virtio devices, the API server runtime, snapshot machinery, and
//! the event loop on top of this skeleton.
//!
//! See [13-arch-and-boot.md § 9](../../../specs/13-arch-and-boot.md#9-boot-orchestration).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Boot-orchestration prose mentions DRAM, FDT, MMIO, MIB-aligned bands and other
// hardware identifiers; backticking each one bloats the docs.
#![allow(clippy::doc_markdown)]
// `std::fs::metadata` for an initrd is a one-shot config-time read (sub-millisecond),
// not a hot path that should yield to tokio. The clippy ban targets runtime code.
#![allow(clippy::disallowed_methods)]

pub mod builder;
pub mod device_manager;
pub mod resources;
#[cfg(target_os = "macos")]
pub mod runner;

pub use builder::{BootArtifacts, BootError, build_microvm_for_boot};
pub use device_manager::{
    BlockConfigSpec, DeviceBuildArgs, DeviceError, DeviceLayout, NetSpec, build_device_layout,
    default_guest_mac,
};
pub use resources::{InitrdSource, KernelSource, VmResources};
#[cfg(target_os = "macos")]
pub use runner::{MicrovmHandle, RunResult, ShutdownReason, run_microvm};
