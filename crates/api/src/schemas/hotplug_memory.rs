//! `/hotplug/memory` PUT and PATCH bodies — virtio-mem hotplug.
//!
//! Per [21-api-compat-matrix.md
//! `/hotplug/memory`](../../../specs/21-api-compat-matrix.md#hotplugmemory) the endpoint accepts
//! `PUT` (full config), `GET` (read-only mirror), and `PATCH` (size update). Field names mirror
//! upstream Firecracker's `MemoryHotplugConfig` and `MemoryHotplugSizeUpdate`.

use serde::{Deserialize, Serialize};

/// Default block size in MiB. Block is the smallest unit the guest can hot(un)plug.
pub const DEFAULT_BLOCK_SIZE_MIB: u64 = 2;

/// Default slot size in MiB. Slot is the smallest unit the host can (de)attach.
pub const DEFAULT_SLOT_SIZE_MIB: u64 = 128;

/// Upper bound on the configured `total_size_mib`. The host-RAM cap is enforced at
/// the controller (Phase 3 territory once the VMM event loop knows the running set).
pub const MAX_TOTAL_SIZE_MIB: u64 = 1_048_576; // 1 TiB

/// Raw `/hotplug/memory` PUT body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHotplugMemoryConfig {
    /// Total memory size in MiB that can be hotplugged.
    pub total_size_mib: u64,
    /// Block size in MiB.
    #[serde(default = "default_block_size")]
    pub block_size_mib: u64,
    /// Slot size in MiB.
    #[serde(default = "default_slot_size")]
    pub slot_size_mib: u64,
}

fn default_block_size() -> u64 {
    DEFAULT_BLOCK_SIZE_MIB
}

fn default_slot_size() -> u64 {
    DEFAULT_SLOT_SIZE_MIB
}

/// Validated `/hotplug/memory` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct HotplugMemoryConfig {
    /// Total memory size in MiB.
    pub total_size_mib: u64,
    /// Block size in MiB.
    pub block_size_mib: u64,
    /// Slot size in MiB.
    pub slot_size_mib: u64,
}

impl TryFrom<RawHotplugMemoryConfig> for HotplugMemoryConfig {
    type Error = String;

    fn try_from(raw: RawHotplugMemoryConfig) -> Result<Self, Self::Error> {
        if raw.total_size_mib == 0 || raw.total_size_mib > MAX_TOTAL_SIZE_MIB {
            return Err(format!(
                "Invalid total_size_mib: must be 1..={MAX_TOTAL_SIZE_MIB}"
            ));
        }
        if raw.block_size_mib == 0 {
            return Err("Invalid block_size_mib: must be >= 1".into());
        }
        if raw.slot_size_mib == 0 {
            return Err("Invalid slot_size_mib: must be >= 1".into());
        }
        if raw.slot_size_mib < raw.block_size_mib {
            return Err("Invalid hotplug-memory: slot_size_mib must be >= block_size_mib".into());
        }
        if !raw.total_size_mib.is_multiple_of(raw.slot_size_mib) {
            return Err("Invalid total_size_mib: must be a multiple of slot_size_mib".into());
        }
        Ok(Self {
            total_size_mib: raw.total_size_mib,
            block_size_mib: raw.block_size_mib,
            slot_size_mib: raw.slot_size_mib,
        })
    }
}

/// Raw `/hotplug/memory` PATCH body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHotplugMemoryUpdate {
    /// New requested size in MiB.
    pub requested_size_mib: u64,
}

/// Validated `/hotplug/memory` PATCH body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct HotplugMemoryUpdate {
    /// New requested size in MiB.
    pub requested_size_mib: u64,
}

impl TryFrom<RawHotplugMemoryUpdate> for HotplugMemoryUpdate {
    type Error = String;

    fn try_from(raw: RawHotplugMemoryUpdate) -> Result<Self, Self::Error> {
        if raw.requested_size_mib > MAX_TOTAL_SIZE_MIB {
            return Err(format!(
                "Invalid requested_size_mib: must be 0..={MAX_TOTAL_SIZE_MIB}"
            ));
        }
        Ok(Self {
            requested_size_mib: raw.requested_size_mib,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_minimal_hotplug_memory_config() {
        let cfg = HotplugMemoryConfig::try_from(RawHotplugMemoryConfig {
            total_size_mib: 256,
            block_size_mib: DEFAULT_BLOCK_SIZE_MIB,
            slot_size_mib: DEFAULT_SLOT_SIZE_MIB,
        })
        .unwrap();
        assert_eq!(cfg.total_size_mib, 256);
    }

    #[test]
    fn test_should_reject_unaligned_total_size() {
        assert!(
            HotplugMemoryConfig::try_from(RawHotplugMemoryConfig {
                total_size_mib: 100,
                block_size_mib: 2,
                slot_size_mib: 128,
            })
            .is_err()
        );
    }

    #[test]
    fn test_should_reject_slot_smaller_than_block() {
        assert!(
            HotplugMemoryConfig::try_from(RawHotplugMemoryConfig {
                total_size_mib: 256,
                block_size_mib: 64,
                slot_size_mib: 32,
            })
            .is_err()
        );
    }

    #[test]
    fn test_should_accept_zero_requested_size_for_unplug_to_zero() {
        let upd = HotplugMemoryUpdate::try_from(RawHotplugMemoryUpdate {
            requested_size_mib: 0,
        })
        .unwrap();
        assert_eq!(upd.requested_size_mib, 0);
    }

    #[test]
    fn test_should_round_trip_patch_through_serde() {
        let raw: RawHotplugMemoryUpdate =
            serde_json::from_str(r#"{"requested_size_mib":512}"#).unwrap();
        assert_eq!(raw.requested_size_mib, 512);
    }
}
