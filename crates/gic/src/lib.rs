//! Thin GICv3 wrapper over `applevisor`'s `hv_gic_*` APIs.
//!
//! `squib-gic` is the only place the SPI assertion shape is defined. Per
//! [99-key-decisions.md § D24](../../../specs/99-key-decisions.md#d24-edge-spi-pulse-shape):
//!
//! - **level-triggered** lines (PL011 UART, virtio-block when `VIRTIO_F_NOTIFY_ON_EMPTY` is
//!   negotiated): the device asserts and de-asserts explicitly via [`Gic::set_spi_level`].
//! - **edge-rising** lines (the default for virtio-MMIO, FDT flag `1`): the device pulses with
//!   [`Gic::pulse_spi`] which is `set_spi(true)` immediately followed by `set_spi(false)`. The
//!   GIC's pending-bit fires once, the controller observes the de-assertion immediately. No
//!   coalescing logic — virtio's queue notifications are idempotent at the cursor level.
//!
//! All `IntId` values are validated through [`squib_arch::IntId`] before reaching HVF.

// `squib-gic` carries no `unsafe` blocks; applevisor's safe API is sufficient.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::doc_markdown)]

#[cfg(target_os = "macos")]
use applevisor::{
    error::HypervisorError,
    gic::GicConfig,
    vm::{GicEnabled, VirtualMachineInstance},
};
use squib_arch::IntId;
use thiserror::Error;

/// Errors that can surface from the GIC layer.
#[derive(Debug, Error)]
pub enum GicError {
    /// Underlying HVF call failed.
    #[error("HVF GIC error: {0}")]
    Hvf(String),
    /// Squib does not run on hosts without HVF.
    #[error("squib-gic requires macOS; this build target lacks HVF")]
    UnsupportedHost,
}

#[cfg(target_os = "macos")]
impl From<HypervisorError> for GicError {
    fn from(err: HypervisorError) -> Self {
        Self::Hvf(format!("{err:?}"))
    }
}

/// Per-host GIC sizing facts, as reported by `hv_gic_get_*_size`.
///
/// Apple may grow these in a future macOS release; squib reads them at boot rather than
/// hard-coding values. The defaults are the macOS 15 values (Apple-Silicon).
#[derive(Debug, Clone, Copy)]
pub struct GicSizes {
    /// Distributor region size — `hv_gic_get_distributor_size`.
    pub distributor: u64,
    /// Redistributor region per-vCPU — `hv_gic_get_redistributor_size`.
    pub redistributor_per_vcpu: u64,
}

impl GicSizes {
    /// Query the GIC sizes from HVF.
    ///
    /// # Errors
    /// [`GicError::Hvf`] on any HVF-side failure.
    #[cfg(target_os = "macos")]
    pub fn query() -> Result<Self, GicError> {
        let distributor = GicConfig::get_distributor_size()? as u64;
        let redistributor_per_vcpu = GicConfig::get_redistributor_size()? as u64;
        Ok(Self {
            distributor,
            redistributor_per_vcpu,
        })
    }

    /// Stub for non-macOS hosts.
    ///
    /// # Errors
    /// Always [`GicError::UnsupportedHost`].
    #[cfg(not(target_os = "macos"))]
    pub fn query() -> Result<Self, GicError> {
        Err(GicError::UnsupportedHost)
    }
}

/// The portable GIC trait surface squib-vmm and device crates consume.
pub trait Gic {
    /// Pulse an edge-rising SPI line: assert, then immediately de-assert. The GIC
    /// observes one pending edge and clears it.
    ///
    /// Use this for virtio-MMIO interrupt delivery (D24).
    ///
    /// # Errors
    /// [`GicError::Hvf`] for any HVF failure.
    fn pulse_spi(&self, intid: IntId) -> Result<(), GicError>;

    /// Drive a level-triggered SPI line. The device is responsible for explicitly
    /// de-asserting when the guest has acknowledged the IRQ at the GIC.
    ///
    /// Use this for PL011 UART RX, virtio-block under `VIRTIO_F_NOTIFY_ON_EMPTY`, etc.
    ///
    /// # Errors
    /// [`GicError::Hvf`] for any HVF failure.
    fn set_spi_level(&self, intid: IntId, level: bool) -> Result<(), GicError>;

    /// Save the GIC state into an opaque blob. The caller serialises this into
    /// `MicrovmState.gic_state`.
    ///
    /// # Errors
    /// [`GicError::Hvf`] for any HVF failure.
    fn save_state(&self) -> Result<Vec<u8>, GicError>;

    /// Restore the GIC state from an opaque blob produced by [`Self::save_state`]. Must
    /// run **before** any `hv_vcpu_run` (per I-SNAP-3); the boot orchestrator gates on
    /// this completing.
    ///
    /// # Errors
    /// [`GicError::Hvf`] for any HVF failure.
    fn restore_state(&self, data: &[u8]) -> Result<(), GicError>;
}

/// HVF-backed GIC implementation. On non-macOS hosts this is a stub that returns
/// [`GicError::UnsupportedHost`] from every method.
#[derive(Debug)]
pub struct HvfGic {
    #[cfg(target_os = "macos")]
    instance: VirtualMachineInstance<GicEnabled>,
}

impl HvfGic {
    /// Build an [`HvfGic`] from a live `applevisor::VirtualMachineInstance<GicEnabled>`.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn new(instance: VirtualMachineInstance<GicEnabled>) -> Self {
        Self { instance }
    }

    /// Stub for non-macOS hosts. The unit at this layer never actually runs on a
    /// non-Apple-Silicon host in practice, but the constructor keeps the cross-target
    /// build green.
    #[cfg(not(target_os = "macos"))]
    #[must_use]
    pub const fn new_stub() -> Self {
        Self {}
    }
}

impl Gic for HvfGic {
    #[cfg(target_os = "macos")]
    fn pulse_spi(&self, intid: IntId) -> Result<(), GicError> {
        // D24: synchronous assert→deassert. virtio's queue cursor is idempotent so
        // a missed pulse self-heals on the next notification; no state machine here.
        self.instance.gic_set_spi(intid.as_raw(), true)?;
        self.instance.gic_set_spi(intid.as_raw(), false)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn set_spi_level(&self, intid: IntId, level: bool) -> Result<(), GicError> {
        self.instance.gic_set_spi(intid.as_raw(), level)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn save_state(&self) -> Result<Vec<u8>, GicError> {
        let mut state = self.instance.gic_state_create()?;
        let size = state.size()?;
        let mut buf = vec![0u8; size];
        state.get(&mut buf)?;
        Ok(buf)
    }

    #[cfg(target_os = "macos")]
    fn restore_state(&self, data: &[u8]) -> Result<(), GicError> {
        // The applevisor crate constructs `GicState` via `gic_state_create` and exposes
        // `set` to install a previously-saved blob. We mirror that flow here so the
        // restore path runs through the same lifecycle as save.
        let state = self.instance.gic_state_create()?;
        state.set(data)?;
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn pulse_spi(&self, _intid: IntId) -> Result<(), GicError> {
        Err(GicError::UnsupportedHost)
    }

    #[cfg(not(target_os = "macos"))]
    fn set_spi_level(&self, _intid: IntId, _level: bool) -> Result<(), GicError> {
        Err(GicError::UnsupportedHost)
    }

    #[cfg(not(target_os = "macos"))]
    fn save_state(&self) -> Result<Vec<u8>, GicError> {
        Err(GicError::UnsupportedHost)
    }

    #[cfg(not(target_os = "macos"))]
    fn restore_state(&self, _data: &[u8]) -> Result<(), GicError> {
        Err(GicError::UnsupportedHost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On non-macOS hosts every Gic method must return UnsupportedHost cleanly.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn stub_methods_return_unsupported_host() {
        let gic = HvfGic::new_stub();
        let intid = IntId::from_spi_cell(16).unwrap();
        assert!(matches!(
            gic.pulse_spi(intid),
            Err(GicError::UnsupportedHost)
        ));
        assert!(matches!(
            gic.set_spi_level(intid, true),
            Err(GicError::UnsupportedHost)
        ));
        assert!(matches!(gic.save_state(), Err(GicError::UnsupportedHost)));
        assert!(matches!(
            gic.restore_state(&[]),
            Err(GicError::UnsupportedHost)
        ));
    }

    #[test]
    fn pulse_spi_requires_validated_intid_via_squib_arch() {
        // Compile-time confirmation: pulse_spi takes IntId, not raw u32. A regression
        // would surface as a build error in this test, not at runtime.
        fn _accepts_intid<G: Gic>(gic: &G, id: IntId) -> Result<(), GicError> {
            gic.pulse_spi(id)
        }
        // No runtime assertion needed — the test passing means the type signature held.
    }
}
