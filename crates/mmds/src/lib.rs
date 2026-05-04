//! MMDS — instance metadata service.
//!
//! Implements the data-store, V2 token, ARP responder, and full
//! IPv4 + TCP + HTTP/1.1 server from
//! [15-mmds.md](../../../specs/15-mmds.md) and the virtio-net
//! `MmdsInterceptor` seam in
//! [14-virtio-and-devices.md § 4.2](../../../specs/14-virtio-and-devices.md#42-virtio-net).
//!
//! ## Module map
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | [`data_store`] | JSON tree + JSON Pointer traversal + size-cap |
//! | [`token`] | V2 token issuer with bounded TTL and constant-time compare |
//! | [`pdu`] | Ethernet + ARP + IPv4 + TCP byte-layout helpers |
//! | [`tcp`] | TCP state machine + HTTP/1.1 parser, the dumbo TCP server |
//! | [`interceptor`] | Frame interceptor implementing the virtio-net seam |
//!
//! ## End-to-end flow
//!
//! 1. Guest emits an ARP request for the MMDS IP → [`MmdsInterceptor`] answers with the synthetic
//!    MAC.
//! 2. Guest opens a TCP connection to `MMDS_IP:80` → [`tcp::TcpServer`] runs the SYN/SYN-ACK/ACK
//!    handshake.
//! 3. Guest sends an HTTP request → the parser hands an [`tcp::HttpRequest`] to the data-store
//!    handler.
//! 4. Handler returns an [`tcp::HttpResponse`] → the server frames it into TCP segments + IPv4 +
//!    Ethernet, queues them for the virtio-net RX path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Network protocol code: casts between u8/u16/u32/usize are the wire
// contract (header lengths, sequence numbers, port widths). The pedantic
// truncation lints are too noisy for code where the cast IS the spec.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::similar_names
)]

pub mod data_store;
pub mod interceptor;
pub mod pdu;
pub mod tcp;
pub mod token;

pub use data_store::{Mmds, MmdsError, MmdsVersion};
pub use interceptor::{MMDS_DEFAULT_IPV4, MMDS_SYNTHETIC_MAC, MmdsInterceptor};
pub use tcp::{HttpRequest, HttpResponse, OutboundFrame, TcpServer};
pub use token::{Token, TokenStore, TokenStoreError};
