//! Host-network mode selectors.
//!
//! [`VmnetMode`] maps 1:1 to `vmnet.framework`'s `vmnet_operation_mode_key` enum.
//! [`NetMode`] is the user-facing CLI choice: vmnet (Shared/Host/Bridged) plus the
//! Userspace option that bundles `gvproxy` instead.

/// Vmnet operating mode. Mirrors `VMNET_SHARED_MODE` / `VMNET_HOST_MODE` /
/// `VMNET_BRIDGED_MODE` — the `u64` discriminants come straight from `<vmnet/vmnet.h>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VmnetMode {
    /// `VMNET_SHARED_MODE` — guests get an IP behind a host-side NAT.
    Shared,
    /// `VMNET_HOST_MODE` — host-only network (no NAT egress).
    Host,
    /// `VMNET_BRIDGED_MODE` — bridged onto a real host interface. Requires the
    /// restricted `com.apple.vm.networking` entitlement; only compiled in when the
    /// `bridged` cargo feature is enabled.
    #[cfg(feature = "bridged")]
    Bridged,
}

impl VmnetMode {
    /// Numeric discriminant used in the XPC dictionary under `vmnet_operation_mode_key`.
    /// The constants come from `<vmnet/vmnet.h>` (`operating_modes_t`) and are
    /// stable across macOS versions.
    #[must_use]
    pub fn as_xpc_value(self) -> u64 {
        match self {
            // VMNET_HOST_MODE = 1000
            Self::Host => 1000,
            // VMNET_SHARED_MODE = 1001
            Self::Shared => 1001,
            // VMNET_BRIDGED_MODE = 1002
            #[cfg(feature = "bridged")]
            Self::Bridged => 1002,
        }
    }
}

/// User-facing network mode. The `Userspace` variant routes through the bundled
/// `gvproxy` child process; the `Vmnet(_)` variants go through `vmnet.framework`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetMode {
    /// vmnet shared/host/bridged.
    Vmnet(VmnetMode),
    /// gvproxy bundled-binary userspace TCP/IP.
    Userspace,
}

impl NetMode {
    /// Convenience: shared (NAT) mode.
    pub const SHARED: Self = Self::Vmnet(VmnetMode::Shared);
    /// Convenience: host-only mode.
    pub const HOST: Self = Self::Vmnet(VmnetMode::Host);
    /// Convenience: gvproxy userspace mode.
    pub const USERSPACE: Self = Self::Userspace;

    /// Convenience: bridged mode (only available when the `bridged` cargo feature is on).
    #[cfg(feature = "bridged")]
    pub const BRIDGED: Self = Self::Vmnet(VmnetMode::Bridged);

    /// Whether this mode requires `com.apple.vm.networking` (the restricted
    /// entitlement). Only bridged does. Per [30-networking.md §
    /// 2](../../../specs/30-networking.md#2-modes).
    #[must_use]
    pub fn needs_restricted_entitlement(self) -> bool {
        match self {
            #[cfg(feature = "bridged")]
            Self::Vmnet(VmnetMode::Bridged) => true,
            Self::Vmnet(_) | Self::Userspace => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_map_vmnet_modes_to_xpc_constants() {
        // Per <vmnet/vmnet.h> operating_modes_t:
        //   VMNET_HOST_MODE = 1000, VMNET_SHARED_MODE = 1001, VMNET_BRIDGED_MODE = 1002.
        assert_eq!(VmnetMode::Host.as_xpc_value(), 1000);
        assert_eq!(VmnetMode::Shared.as_xpc_value(), 1001);
    }

    #[test]
    fn test_should_only_flag_bridged_as_restricted_entitlement() {
        assert!(!NetMode::Vmnet(VmnetMode::Shared).needs_restricted_entitlement());
        assert!(!NetMode::Vmnet(VmnetMode::Host).needs_restricted_entitlement());
        assert!(!NetMode::Userspace.needs_restricted_entitlement());
    }

    #[cfg(feature = "bridged")]
    #[test]
    fn test_should_flag_bridged_as_restricted() {
        assert_eq!(VmnetMode::Bridged.as_xpc_value(), 1002);
        assert!(NetMode::Vmnet(VmnetMode::Bridged).needs_restricted_entitlement());
    }
}
