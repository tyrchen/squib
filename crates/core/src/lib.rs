//! Portable types and traits at the heart of squib.
//!
//! This crate intentionally has zero OS dependencies: it defines the abstractions
//! that backend crates (`squib-vz`, `squib-hvf`) implement, the device traits the
//! VMM crate consumes, and the value types that flow through the API server. See
//! `specs/squib-design.md` for the architectural picture.
//!
//! The crate is organized as:
//!
//! - [`error`] — the [`Error`] enum every fallible operation in squib returns.
//! - [`memory`] — guest-physical address ranges and memory protection.
//! - [`exit`] — the [`VmExit`] enum that subsumes KVM's `VcpuExit` and HVF/VZ exit shapes.
//! - [`vcpu`] — register file, interrupt descriptors, and the [`Vcpu`] trait.
//! - [`backend`] — the [`HypervisorBackend`] / [`Vm`] trait pair plus capability discovery.
//! - [`lifecycle`] — the internal [`LifecyclePhase`] state machine and the wire-shape
//!   [`WireVmState`] surfaced by `GET /`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backend;
pub mod error;
pub mod exit;
pub mod lifecycle;
pub mod memory;
pub mod vcpu;

pub use backend::{BackendCapabilities, BackendKind, HypervisorBackend, MAX_SUPPORTED_VCPUS, Vm};
pub use error::{Error, Result};
pub use exit::{DebugInfo, VmExit};
pub use lifecycle::{LifecyclePhase, WireVmState};
pub use memory::{GuestAddress, GuestMemoryRegion, GuestRange, Protection};
pub use vcpu::{Irq, Regs, Vcpu};
