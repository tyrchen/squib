//! Firecracker-compatible HTTP API server for squib.
//!
//! `squib-api` exposes the same `OpenAPI` surface that upstream Firecracker speaks over a
//! Unix domain socket: same paths, same JSON shapes, same status codes, same
//! `{"fault_message": "..."}` error body, same `Server: Firecracker API` response header.
//! See `specs/squib-api-compat-design.md` for the per-endpoint matrix.
//!
//! This crate intentionally has no dependency on the hypervisor backend. Handlers call
//! through a [`Runtime`] trait that the VMM crate (`squib-vmm`) implements; tests provide
//! their own mock runtime so the wire layer can be exercised without HVF.
//!
//! # Layout
//!
//! - [`error`] — the [`error::FaultMessage`] body and [`error::ApiError`] response type.
//! - [`schemas`] — request/response structs that mirror Firecracker's `OpenAPI` shapes.
//! - [`server`] — the axum router, the [`server::Runtime`] trait, the [`server::serve`] entry
//!   point.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod schemas;
pub mod server;

pub use error::{ApiError, FaultMessage, Result};
pub use schemas::{InstanceInfo, VersionResponse, VmState};
pub use server::{Runtime, ServeOptions, serve};
