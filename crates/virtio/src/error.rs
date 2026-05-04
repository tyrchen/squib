//! Crate-level error type for squib-virtio.
//!
//! Per CLAUDE.md `§ Error Handling`, we use `thiserror` and a single
//! crate-rooted enum so callers can `?` from any submodule without
//! pre-mapping. Variants are non-exhaustive so future device modules can add
//! their own without forcing every consumer to recompile match arms.

use thiserror::Error;

/// All fallible operations in `squib-virtio` return [`Result<T, VirtioError>`].
pub type Result<T> = core::result::Result<T, VirtioError>;

/// Errors produced by the virtio transport, queue handling, or device frontends.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VirtioError {
    /// Guest memory access failed (range escape, region overflow).
    #[error(transparent)]
    Memory(#[from] squib_core::Error),

    /// A queue setup or descriptor-walk error.
    #[error(transparent)]
    Queue(#[from] crate::queue::QueueError),

    /// Failed to deliver an IRQ to the GIC.
    #[error("GIC delivery failed: {0}")]
    Irq(String),

    /// Device-level activation failure (resource unavailable, configuration
    /// mismatch).
    #[error(transparent)]
    Activate(#[from] crate::device::ActivateError),

    /// I/O backend error (host file, network socket, ...).
    #[error("device I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Device received a malformed descriptor or request payload.
    #[error("malformed payload: {0}")]
    MalformedPayload(String),

    /// Device received a write to a register that is invalid in the current
    /// state machine state (e.g. configuring a queue after `DRIVER_OK`).
    #[error("invalid driver-init transition: {0}")]
    InvalidTransition(&'static str),
}

impl From<squib_gic::GicError> for VirtioError {
    fn from(err: squib_gic::GicError) -> Self {
        Self::Irq(err.to_string())
    }
}
