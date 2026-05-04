//! `MicrovmState` and child types — the snapshot state blob.
//!
//! Per [10-data-model.md §
//! 5](../../../specs/10-data-model.md#5-microvmstate--the-snapshot-state-blob) and [16-snapshots.md
//! § 2](../../../specs/16-snapshots.md#2-state-file). These types are the source of truth for the
//! on-disk format; the [`crate::envelope::Snapshot`] envelope wraps an instance of [`MicrovmState`]
//! and the bitcode encoding pins the field order.
//!
//! All fields are `serde`-serializable. Adding a register, a device-state field, or
//! an MMDS V2 token field is an additive contract: the snapshot version's `minor`
//! must bump in lockstep with upstream Firecracker (D15 + D6); fields are never
//! removed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::SnapshotError;

/// Per-vCPU general-purpose register file.
///
/// Layout per [13-arch-and-boot.md §
/// 8](../../../specs/13-arch-and-boot.md#8-vcpu-initial-registers). Encoding is by index —
/// `regs[0]` is X0, `regs[30]` is X30. `sp`, `pc`, `pstate` are split out for readability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpRegs {
    /// X0..X30 (31 registers).
    pub x: [u64; 31],
    /// SP_EL1 — used by the postcopy pre-warmer to resolve the boot stack page.
    pub sp: u64,
    /// PC at save time.
    pub pc: u64,
    /// PSTATE / SPSR_EL2.
    pub pstate: u64,
}

/// Per-vCPU FP/SIMD register file (V0..V31, FPSR, FPCR).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FpSimdRegs {
    /// V0..V31 — 32 registers, each 128 bits (split into two `u64` for `serde`).
    pub v: [[u64; 2]; 32],
    /// FPSR — floating-point status register.
    pub fpsr: u64,
    /// FPCR — floating-point control register.
    pub fpcr: u64,
}

/// PSCI affinity state for a vCPU at snapshot time.
///
/// On restore, every vCPU's PSCI state is normalized to BSP-running /
/// secondaries-Off; this field is captured for diagnostics and so the restore
/// orchestrator knows which vCPUs were live before save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PsciVcpuState {
    /// vCPU was active.
    On,
    /// vCPU was parked (CPU_OFF / never CPU_ON-ed).
    #[default]
    Off,
    /// vCPU is mid-bringup (CPU_ON received, primary not yet running).
    OnPending,
}

/// Per-vCPU state blob.
///
/// Pin per [10-data-model.md §
/// 5](../../../specs/10-data-model.md#5-microvmstate--the-snapshot-state-blob).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuState {
    /// MPIDR_EL1 affinity bits assigned at create time.
    pub mpidr: u64,
    /// General-purpose register file.
    pub regs: GpRegs,
    /// FP/SIMD register file.
    pub fp_regs: FpSimdRegs,
    /// Curated sysreg subset — `BTreeMap<u64, u64>` so the encoding is order-stable.
    /// The key is the [`squib_arch::SysReg`]'s `as_encoded()` value (a `u64` packed
    /// representation).
    pub sys_regs: BTreeMap<u64, u64>,
    /// PSCI affinity state at save time.
    pub psci_state: PsciVcpuState,
}

impl VcpuState {
    /// Construct a fresh `VcpuState` for the given MPIDR with all registers cleared.
    #[must_use]
    pub fn new(mpidr: u64) -> Self {
        Self {
            mpidr,
            regs: GpRegs::default(),
            fp_regs: FpSimdRegs::default(),
            sys_regs: BTreeMap::new(),
            psci_state: PsciVcpuState::Off,
        }
    }
}

/// Opaque GIC state blob from `hv_gic_state_get_data`.
///
/// The shape is HVF-specific; restoring a snapshot taken on KVM (e.g. on Linux
/// aarch64) into squib is explicitly not supported (D10), so the bytes flow through
/// as opaque. The `len` field is redundant with `bytes.len()` on the wire but keeps
/// `serde_json::to_value` (used for `--describe-snapshot`) self-documenting.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GicState {
    /// Length of the opaque blob (bytes).
    pub len: u64,
    /// The blob itself.
    pub bytes: Vec<u8>,
}

impl GicState {
    /// Build a `GicState` from the bytes returned by `hv_gic_state_get_data`.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        // `usize::try_from` to `u64` is infallible on 64-bit, but going through it
        // surfaces the conversion in the type system — a 32-bit target would catch
        // the overflow at compile time instead of silently truncating at runtime.
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Self { len, bytes }
    }
}

/// MMDS data store + V2 token store, captured at snapshot time.
///
/// The store is held as a **JSON-serialized string** (not `serde_json::Value`)
/// because the snapshot envelope encodes via `bitcode`, which does not implement
/// `deserialize_any` and therefore cannot decode `serde_json::Value` directly.
/// Use [`Self::data_value`] / [`Self::with_data`] to round-trip through a
/// structured value.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MmdsState {
    /// The MMDS data store, JSON-serialized. `"null"` for an empty store.
    pub data_json: String,
    /// V2 session-token TTL, in seconds. `None` means MMDS V1 was active.
    pub token_ttl_seconds: Option<u32>,
}

impl MmdsState {
    /// Build an `MmdsState` from a structured `serde_json::Value`.
    ///
    /// # Errors
    /// Surfaces any `serde_json::Error` from re-serializing the value.
    pub fn with_data(
        value: &serde_json::Value,
        token_ttl_seconds: Option<u32>,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            data_json: serde_json::to_string(value)?,
            token_ttl_seconds,
        })
    }

    /// Decode `data_json` back to a `serde_json::Value`.
    ///
    /// # Errors
    /// Surfaces any `serde_json::Error` from parsing.
    pub fn data_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        if self.data_json.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&self.data_json)
    }
}

/// Top-level VM info: `mem_size_mib`, `smt`, CPU template, boot source.
///
/// Captured separately from per-vCPU state so the loader can validate "would this
/// state file fit on this host" before allocating guest RAM.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VmInfo {
    /// Configured guest RAM in MiB (matches `machine-config.mem_size_mib`).
    pub mem_size_mib: u64,
    /// Always `false` on aarch64 — SMT is not exposed to guests on Apple Silicon.
    pub smt: bool,
    /// CPU template name (e.g. `"V1N1"`); empty if the user did not pin one.
    pub cpu_template: String,
    /// Boot-source `kernel_image_path` recorded for diagnostics.
    pub kernel_image_path: String,
    /// Boot-source `initrd_path` if any.
    pub initrd_path: Option<String>,
    /// Effective `boot_args` after the D23 append-if-absent rules ran.
    pub boot_args: String,
    /// Whether dirty-page tracking was enabled when the snapshot was taken.
    /// A `Diff` snapshot is only legal when this is `true` (I-SNAP-6).
    pub track_dirty_pages: bool,
}

/// One device's saved state — config + virtqueue cursors.
///
/// Devices serialise their own state into bitcode-friendly shapes; the snapshot
/// crate keeps the outer `BTreeMap<DeviceKey, DeviceState>` so the order is stable
/// across save/restore (per virtio-MMIO slot). The inner blob is opaque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    /// Device-class name (`"virtio-block"`, `"virtio-net"`, ...).
    pub kind: String,
    /// Identifier the API surface uses (e.g. `drive_id`, `iface_id`).
    pub id: String,
    /// virtio-MMIO slot index this device occupied.
    pub mmio_slot: u32,
    /// Opaque per-device state — bitcode bytes the device emits.
    pub blob: Vec<u8>,
}

/// All device states, keyed by `(mmio_slot, id)` for stable ordering.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeviceStates {
    /// Devices in MMIO-slot order.
    pub devices: Vec<DeviceState>,
}

impl DeviceStates {
    /// Build a `DeviceStates` from an iterable of `DeviceState`.
    ///
    /// The constructor sorts by `(mmio_slot, id)` so the wire encoding is stable
    /// across HashMap iteration order and across runs.
    pub fn from_devices<I: IntoIterator<Item = DeviceState>>(iter: I) -> Self {
        let mut devices: Vec<_> = iter.into_iter().collect();
        devices.sort_by(|a, b| (a.mmio_slot, a.id.as_str()).cmp(&(b.mmio_slot, b.id.as_str())));
        Self { devices }
    }
}

/// The snapshot state blob — the `data` field of [`crate::envelope::Snapshot`].
///
/// Pin per [10-data-model.md §
/// 5](../../../specs/10-data-model.md#5-microvmstate--the-snapshot-state-blob).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MicrovmState {
    /// Top-level VM info.
    pub vm_info: VmInfo,
    /// One per vCPU.
    pub vcpu_states: Vec<VcpuState>,
    /// Per-device state.
    pub device_states: DeviceStates,
    /// Opaque GIC blob.
    pub gic_state: GicState,
    /// MMDS state if MMDS was enabled.
    pub mmds_state: Option<MmdsState>,
}

impl MicrovmState {
    /// Squib-incompat sanity check — surfaces [`SnapshotError::Incompatible`] when
    /// the loaded state could not have come from a squib-shaped VMM.
    ///
    /// Used by the loader after a successful magic + CRC + version check to give
    /// `--describe-snapshot` a clean rejection of e.g. KVM-produced state files
    /// that *happen* to deserialise structurally (D10).
    ///
    /// # Errors
    /// [`SnapshotError::Incompatible`] if any structural invariant is violated.
    pub fn verify_compatible(&self) -> Result<(), SnapshotError> {
        // vCPU count must match the saved per-vCPU state (1..=32 per D19).
        if self.vcpu_states.is_empty() || self.vcpu_states.len() > 32 {
            return Err(SnapshotError::Incompatible);
        }
        // SMT must be false on aarch64 (D3).
        if self.vm_info.smt {
            return Err(SnapshotError::Incompatible);
        }
        // GIC blob must be non-empty for a saved-running VM.
        if self.gic_state.bytes.is_empty() {
            return Err(SnapshotError::Incompatible);
        }
        // GIC blob length redundancy must agree with the byte vector.
        if usize::try_from(self.gic_state.len).unwrap_or(usize::MAX) != self.gic_state.bytes.len() {
            return Err(SnapshotError::Incompatible);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
        value: &T,
    ) -> T {
        let bytes = bitcode::serialize(value).expect("encode");
        bitcode::deserialize(&bytes).expect("decode")
    }

    #[test]
    fn test_should_round_trip_default_microvm_state() {
        let state = MicrovmState {
            vm_info: VmInfo {
                mem_size_mib: 256,
                smt: false,
                cpu_template: String::new(),
                kernel_image_path: "/tmp/vmlinux".into(),
                initrd_path: None,
                boot_args: "console=ttyAMA0 panic=1".into(),
                track_dirty_pages: false,
            },
            vcpu_states: vec![VcpuState::new(0)],
            device_states: DeviceStates::default(),
            gic_state: GicState::from_bytes(vec![1, 2, 3, 4]),
            mmds_state: None,
        };
        let back = round_trip(&state);
        assert_eq!(state, back);
    }

    #[test]
    fn test_should_sort_device_states_by_slot_then_id() {
        let states = DeviceStates::from_devices(vec![
            DeviceState {
                kind: "virtio-net".into(),
                id: "eth0".into(),
                mmio_slot: 2,
                blob: vec![],
            },
            DeviceState {
                kind: "virtio-block".into(),
                id: "rootfs".into(),
                mmio_slot: 0,
                blob: vec![],
            },
            DeviceState {
                kind: "virtio-block".into(),
                id: "data".into(),
                mmio_slot: 1,
                blob: vec![],
            },
        ]);
        assert_eq!(states.devices[0].id, "rootfs");
        assert_eq!(states.devices[1].id, "data");
        assert_eq!(states.devices[2].id, "eth0");
    }

    #[test]
    fn test_should_reject_zero_vcpu_state() {
        let state = MicrovmState {
            vm_info: VmInfo {
                mem_size_mib: 1,
                smt: false,
                cpu_template: String::new(),
                kernel_image_path: String::new(),
                initrd_path: None,
                boot_args: String::new(),
                track_dirty_pages: false,
            },
            vcpu_states: vec![],
            device_states: DeviceStates::default(),
            gic_state: GicState::from_bytes(vec![1]),
            mmds_state: None,
        };
        assert!(matches!(
            state.verify_compatible(),
            Err(SnapshotError::Incompatible)
        ));
    }

    #[test]
    fn test_should_reject_smt_enabled() {
        let state = MicrovmState {
            vm_info: VmInfo {
                mem_size_mib: 1,
                smt: true,
                cpu_template: String::new(),
                kernel_image_path: String::new(),
                initrd_path: None,
                boot_args: String::new(),
                track_dirty_pages: false,
            },
            vcpu_states: vec![VcpuState::new(0)],
            device_states: DeviceStates::default(),
            gic_state: GicState::from_bytes(vec![1]),
            mmds_state: None,
        };
        assert!(matches!(
            state.verify_compatible(),
            Err(SnapshotError::Incompatible)
        ));
    }

    #[test]
    fn test_should_reject_empty_gic_blob() {
        let state = MicrovmState {
            vm_info: VmInfo::default(),
            vcpu_states: vec![VcpuState::new(0)],
            device_states: DeviceStates::default(),
            gic_state: GicState::default(),
            mmds_state: None,
        };
        assert!(matches!(
            state.verify_compatible(),
            Err(SnapshotError::Incompatible)
        ));
    }

    #[test]
    fn test_should_reject_gic_length_mismatch() {
        let mut state = MicrovmState {
            vm_info: VmInfo::default(),
            vcpu_states: vec![VcpuState::new(0)],
            device_states: DeviceStates::default(),
            gic_state: GicState::from_bytes(vec![1, 2, 3]),
            mmds_state: None,
        };
        state.gic_state.len = 99;
        assert!(matches!(
            state.verify_compatible(),
            Err(SnapshotError::Incompatible)
        ));
    }

    #[test]
    fn test_should_round_trip_populated_mmds_state() {
        // Regression: an earlier draft stored `data` as `serde_json::Value`,
        // which bitcode cannot decode (requires `deserialize_any`). The smoke
        // test caught this when the demo state file refused to round-trip.
        // The fix stores MMDS as a JSON string; this test pins it.
        let mmds = MmdsState::with_data(
            &serde_json::json!({"latest": {"meta-data": {"instance-id": "demo"}}}),
            Some(3600),
        )
        .unwrap();
        let state = MicrovmState {
            vm_info: VmInfo {
                mem_size_mib: 64,
                smt: false,
                cpu_template: String::new(),
                kernel_image_path: "/k".into(),
                initrd_path: None,
                boot_args: String::new(),
                track_dirty_pages: false,
            },
            vcpu_states: vec![VcpuState::new(0)],
            device_states: DeviceStates::default(),
            gic_state: GicState::from_bytes(vec![0xAA; 16]),
            mmds_state: Some(mmds),
        };
        let back = round_trip(&state);
        let restored = back.mmds_state.expect("MMDS round-trip dropped");
        assert_eq!(restored.token_ttl_seconds, Some(3600));
        let value = restored.data_value().unwrap();
        assert_eq!(value["latest"]["meta-data"]["instance-id"], "demo");
    }

    #[test]
    fn test_should_round_trip_psci_state() {
        for s in [
            PsciVcpuState::On,
            PsciVcpuState::Off,
            PsciVcpuState::OnPending,
        ] {
            let back = round_trip(&s);
            assert_eq!(back, s);
        }
    }
}
