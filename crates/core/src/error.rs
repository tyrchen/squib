//! Error type for portable squib operations.

use thiserror::Error;

/// The result alias every fallible operation in `squib-core` and downstream crates returns.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors that can surface from a hypervisor backend, a vCPU, or a memory operation.
///
/// Variants are intentionally coarse — backend-specific error context is captured in the
/// `source` field of [`Error::Backend`] so consumers can downcast without coupling to a
/// specific backend's error enum.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A backend (HVF, VZ, mock) reported a failure while executing a hypervisor operation.
    #[error("hypervisor backend error: {message}")]
    Backend {
        /// Human-readable message from the backend.
        message: String,
        /// Optional underlying error returned by the backend SDK.
        #[source]
        source: Option<Box<dyn core::error::Error + Send + Sync + 'static>>,
    },

    /// A guest memory operation referenced an address or range that is not mapped.
    #[error("guest memory operation out of range: {0}")]
    MemoryOutOfRange(String),

    /// A guest memory operation requested an unsupported protection change.
    #[error("invalid memory protection change: {0}")]
    InvalidProtection(String),

    /// The capability the caller requested is not implemented by the active backend.
    #[error("backend does not support capability: {0}")]
    Unsupported(&'static str),

    /// A vCPU was driven from an unexpected lifecycle state.
    #[error("invalid vCPU state transition: {0}")]
    InvalidVcpuState(&'static str),

    /// The argument supplied by the caller was rejected by validation.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl Error {
    /// Build a [`Error::Backend`] from a static message.
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend {
            message: message.into(),
            source: None,
        }
    }

    /// Build a [`Error::Backend`] from a message and a wrapped source error.
    pub fn backend_source<E>(message: impl Into<String>, source: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::Backend {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_error_carries_message() {
        let err = Error::backend("failed to map");
        assert_eq!(err.to_string(), "hypervisor backend error: failed to map");
    }

    #[test]
    fn backend_error_chains_source() {
        let inner = Error::MemoryOutOfRange("0x1000".into());
        let outer = Error::backend_source("wrap", inner);
        let chained = core::error::Error::source(&outer)
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(chained.contains("0x1000"));
    }
}
