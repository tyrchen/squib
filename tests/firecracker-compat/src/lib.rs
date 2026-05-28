//! Firecracker compatibility test harness for squib.
//!
//! This crate hosts the **compat suite** required by [91-impl-plan.md § 10
//! Phase 7.1](../../../specs/91-impl-plan.md#10-phase-7-compat-suite-perf-and-polish)
//! and [72-testing-strategy.md § 3](../../../specs/72-testing-strategy.md#3-compat-suite).
//!
//! Each row in [21-api-compat-matrix.md](../../../specs/21-api-compat-matrix.md) gets a
//! parity test:
//!
//! - **F rows** — pass-through replay of an upstream-shaped HTTP request, asserting the documented
//!   status + body.
//! - **P rows** — assert the documented squib deviation (e.g. `host_dev_name` ↔ vmnet handle name).
//! - **A rows** — assert the field is accepted, the warning is emitted, and the VM otherwise
//!   progresses.
//! - **R rows** — assert a 400 response with the documented `fault_message` substring.
//!
//! # Why no real upstream Firecracker process is involved
//!
//! Per [72-testing-strategy.md § 7](../../../specs/72-testing-strategy.md#7-test-discipline):
//! *no mocked vCPU runs in the compat suite*. The harness drives the real squib API
//! crate (the same `axum::Router` the production binary builds) over a real Unix
//! domain socket. The VMM event loop is replaced by a minimal acker so endpoints that
//! require a live VMM (`InstanceStart`, `PUT /snapshot/*`) get a deterministic stub
//! response — mirroring the shape `crates/squib/src/lib.rs` exposes today. Live-VM
//! parity is a Phase 1/3 deliverable; this suite verifies *wire-shape* parity.
//!
//! # Layout
//!
//! - [`server`] — UDS server harness (binds, drains, hangs up cleanly).
//! - [`http`] — minimal raw-HTTP/1.1 client built on `tokio::net::UnixStream` + `httparse`. No
//!   higher-level HTTP client — the wire shape is exactly what an SDK would observe.
//! - [`transcript`] — declarative request/response sequences; replay runs each step against a
//!   single live server.
//! - [`assertions`] — small helpers asserting the upstream invariants (`Server` header,
//!   `fault_message` body shape, status code class).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod assertions;
pub mod http;
pub mod server;
pub mod transcript;

pub use assertions::{assert_fault_message_contains, assert_firecracker_server_header};
pub use http::{HttpResponse, http_request, parse_response};
pub use server::{CompatServer, StubBehaviour};
pub use transcript::{ExpectedResponse, Step, Transcript, replay};
