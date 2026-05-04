//! Device manager — assembles the MMIO bus, virtio frontends, MMDS
//! interceptor, and the `VirtioSlot` vector the FDT consumes.
//!
//! The manager is the single boot-time orchestrator that turns a
//! validated [`crate::resources::VmResources`] plus a live `HvfVm` /
//! `Gic` / guest-memory handle into:
//!
//! - an [`Arc<Bus>`] containing PL011 + virtio-* transports at their canonical addresses,
//! - a [`Vec<VirtioSlot>`] in the order they were allocated, suitable for
//!   [`squib_fdt::FdtBuildArgs::virtio_devices`],
//! - a clone-shareable [`MmdsInterceptor`] handle so the API layer can patch the data store / token
//!   store post-construction,
//! - a `Pl011Sink`-compatible handle (kept by the caller) for capturing the guest's serial output.
//!
//! The manager intentionally does **not** spawn vCPU threads — that is
//! [`crate::runner::run_microvm`]'s job. Separating "build" from "run"
//! keeps the test surface unit-friendly: tests construct a manager,
//! poke the bus directly, and never touch HVF.

// `gic` and `mem` are passed by value so the manager owns the Arcs and
// can clone freely into each per-device transport. The clippy
// `needless_pass_by_value` lint flags this because the function body
// only `Arc::clone`s them; we want the by-value shape so callers don't
// have to keep a separate Arc handle alive.
#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;

use parking_lot::Mutex;
use squib_arch::IntId;
use squib_bus::{Bus, BusBuilder, BusDevice, BusError};
use squib_core::{GuestMemory, HostDevName};
use squib_fdt::VirtioSlot;
use squib_gic::Gic;
use squib_legacy::{Pl011, Pl011Sink};
use squib_mmds::{Mmds, MmdsInterceptor, TokenStore};
use squib_net::{LoopbackHostBackend, NetHostBackend};
use squib_virtio::{
    VirtioDevice,
    devices::{
        block::{BlockConfig, BlockDevice, CacheType, SyncFileBackend},
        console::{ConsoleDevice, ConsoleSink},
        net::{NetBackend, NetConfig, NetDevice},
        rng::{OsEntropy, RngDevice},
    },
    interrupt::IrqLine,
    slot::{Slot, SlotAllocator},
    transport::VirtioMmioTransport,
};
use thiserror::Error;

/// Errors produced by the device manager.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeviceError {
    /// Bus-level overlap or invalid range.
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    /// Slot allocator exhausted (>= 32 virtio devices).
    #[error("virtio slot allocator exhausted: {0}")]
    SlotExhausted(String),

    /// Per-class API cap (drives:8, NICs:8, etc.) violated.
    #[error("device-class cap exceeded: {class} (max {max})")]
    ClassCapExceeded {
        /// Device class name.
        class: &'static str,
        /// Configured maximum.
        max: usize,
    },

    /// Couldn't open a backing file (block / pmem path).
    #[error("file backend failed: {0}")]
    Backend(#[from] std::io::Error),

    /// Couldn't construct an `IntId` for a slot.
    #[error("INTID allocation failed: {0}")]
    IntId(String),

    /// Couldn't construct the OS entropy source for virtio-rng.
    #[error("entropy source failed: {0}")]
    Entropy(String),
}

/// What the manager produces.
pub struct DeviceLayout {
    /// MMIO bus to dispatch reads / writes against.
    pub bus: Arc<Bus>,
    /// Slot descriptors in allocation order — feed straight into the
    /// FDT builder.
    pub virtio_slots: Vec<VirtioSlot>,
    /// MMDS interceptor; the API layer patches its data / token stores
    /// at runtime.
    pub mmds: MmdsInterceptor,
}

impl std::fmt::Debug for DeviceLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceLayout")
            .field("bus_devices", &self.bus.len())
            .field("virtio_slots", &self.virtio_slots.len())
            .finish_non_exhaustive()
    }
}

/// Inputs to [`build_device_layout`].
pub struct DeviceBuildArgs {
    /// Sink for the guest's PL011 console output.
    pub pl011_sink: Box<dyn Pl011Sink>,
    /// MMDS data-store size cap (`/mmds-config { size_limit }`).
    pub mmds_size_cap: usize,
    /// Optional block-device config — loads `/dev/vda`.
    pub block: Option<BlockConfigSpec>,
    /// Optional net config. `None` ⇒ no virtio-net device (MMDS still constructed
    /// for API-side population; just unbound). `Some(NetSpec { backend: Loopback })`
    /// keeps the prior unit-test behaviour. Per
    /// [30-networking.md § 2](../../../specs/30-networking.md#2-modes).
    pub net: Option<NetSpec>,
    /// Whether to expose virtio-console as a secondary console.
    pub enable_console: bool,
}

/// virtio-net configuration plus the host-side backend choice.
pub struct NetSpec {
    /// Operator-supplied iface_id (`/network-interfaces/{id}`).
    pub iface_id: String,
    /// Caller-supplied host device name (informational; the vmnet handle is derived).
    /// `HostDevName` (squib-core) carries the boundary validation in the type system,
    /// so a future direct construction of `NetSpec` cannot bypass the API-layer
    /// length / NUL check (see `93-improvements-review.md` Phase 4 entry).
    pub host_dev_name: HostDevName,
    /// Optional explicit guest MAC; falls back to a stable locally-administered
    /// default if absent.
    pub guest_mac: Option<[u8; 6]>,
    /// Optional explicit MTU; vmnet's reported value is used if `None`.
    pub mtu: Option<u16>,
    /// Host-side backend (vmnet / gvproxy / loopback). Loopback is the test
    /// default and the deterministic non-macOS fallback.
    pub backend: NetHostBackend,
}

impl std::fmt::Debug for NetSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetSpec")
            .field("iface_id", &self.iface_id)
            .field("host_dev_name", &self.host_dev_name.as_str())
            .field("guest_mac", &self.guest_mac)
            .field("mtu", &self.mtu)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for DeviceBuildArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceBuildArgs")
            .field("mmds_size_cap", &self.mmds_size_cap)
            .field("block", &self.block.is_some())
            .field("net", &self.net.is_some())
            .field("enable_console", &self.enable_console)
            .finish_non_exhaustive()
    }
}

/// Spec for the boot block device — squib fills in the rest from the
/// host file's metadata at activation time.
#[derive(Debug, Clone)]
pub struct BlockConfigSpec {
    /// Operator-supplied identifier (`drive_id`).
    pub id: String,
    /// Path on the host filesystem to the rootfs file.
    pub path: std::path::PathBuf,
    /// `true` opens the file read-only.
    pub read_only: bool,
}

/// Build the bus, plug devices into it, and return the slot descriptors
/// the FDT builder needs.
///
/// `gic` is used by every device to deliver IRQs (PL011 RX, virtio
/// queue completions). `mem` is the guest-memory handle; virtio devices
/// consume it for descriptor walks and DMA. Both are `Arc`-wrapped so
/// every device can hold its own clone.
///
/// # Errors
/// [`DeviceError`] for any per-device construction failure.
#[allow(
    clippy::too_many_lines,
    reason = "linear sequence of device constructors; splitting hides the canonical boot-time \
              order documented in 14-virtio-and-devices.md"
)]
pub fn build_device_layout(
    gic: Arc<dyn Gic + Send + Sync>,
    mem: Arc<dyn GuestMemory>,
    args: DeviceBuildArgs,
) -> Result<DeviceLayout, DeviceError> {
    let mut builder = BusBuilder::new();
    let mut allocator = SlotAllocator::new();
    let mut slots: Vec<VirtioSlot> = Vec::new();

    // PL011 — fixed at squib_arch::layout::PL011_BASE, INTID 33 (FDT SPI cell 1).
    {
        let pl011 = Pl011::new(
            args.pl011_sink,
            Arc::clone(&gic),
            IntId::from_spi_cell(1).map_err(|e| DeviceError::IntId(e.to_string()))?,
        );
        let dev: Arc<Mutex<dyn BusDevice>> = Arc::new(Mutex::new(pl011));
        builder.insert(dev, squib_arch::layout::PL011_BASE, 0x1000)?;
    }

    // virtio-rng — every modern Linux kernel asks for entropy at boot;
    // no virtio-rng → the kernel hangs while it gathers entropy from
    // jitter sources and DMA timing. Always include it.
    let rng_slot = alloc_or_fail(&mut allocator)?;
    let rng = RngDevice::with_source(Arc::new(
        OsEntropy::try_new().map_err(|e| DeviceError::Entropy(e.to_string()))?,
    ));
    plug_virtio(
        &mut builder,
        rng_slot,
        Arc::clone(&gic),
        Arc::clone(&mem),
        rng,
    )?;
    slots.push(VirtioSlot {
        slot: rng_slot.index,
    });

    // virtio-block — optional rootfs.
    if let Some(spec) = &args.block {
        let backend = SyncFileBackend::open(&spec.path, spec.read_only)?;
        let block = BlockDevice::new(
            BlockConfig {
                drive_id: spec.id.clone(),
                path_on_host: spec.path.clone(),
                is_root_device: true,
                is_read_only: spec.read_only,
                cache_type: CacheType::Writeback,
                partuuid: None,
            },
            Arc::new(backend),
        );
        let block_slot = alloc_or_fail(&mut allocator)?;
        plug_virtio(
            &mut builder,
            block_slot,
            Arc::clone(&gic),
            Arc::clone(&mem),
            block,
        )?;
        slots.push(VirtioSlot {
            slot: block_slot.index,
        });
    }

    // MMDS — even if virtio-net is disabled we keep the interceptor
    // alive so the API layer can populate it; only when net is enabled
    // does it actually receive frames.
    let mmds = MmdsInterceptor::new(Mmds::new(args.mmds_size_cap), TokenStore::new());

    if let Some(spec) = args.net {
        let net_slot = alloc_or_fail(&mut allocator)?;
        // If the backend is a live vmnet interface, clamp the operator-supplied
        // MTU to vmnet's negotiated value. Setting a virtio MTU above vmnet's
        // would cause the guest to emit oversize frames that vmnet rejects with
        // VMNET_PACKET_TOO_BIG.
        let effective_mtu: u16 = match (&spec.backend, spec.mtu) {
            (NetHostBackend::Vmnet(b), Some(m)) => {
                u16::try_from(b.iface().mtu()).unwrap_or(1500).min(m)
            }
            (NetHostBackend::Vmnet(b), None) => u16::try_from(b.iface().mtu()).unwrap_or(1500),
            (_, Some(m)) => m,
            (_, None) => 1500,
        };
        let backend: Arc<dyn NetBackend> = Arc::new(spec.backend);
        let net = NetDevice::new(
            NetConfig {
                iface_id: spec.iface_id,
                host_dev_name: spec.host_dev_name.into_string(),
                guest_mac: Some(spec.guest_mac.unwrap_or_else(default_guest_mac)),
                mtu: Some(effective_mtu),
            },
            backend,
            Arc::new(mmds.clone()),
        );
        plug_virtio(
            &mut builder,
            net_slot,
            Arc::clone(&gic),
            Arc::clone(&mem),
            net,
        )?;
        slots.push(VirtioSlot {
            slot: net_slot.index,
        });
    }

    if args.enable_console {
        let console_slot = alloc_or_fail(&mut allocator)?;
        let console = ConsoleDevice::new(ConsoleSink::Discard);
        plug_virtio(
            &mut builder,
            console_slot,
            Arc::clone(&gic),
            Arc::clone(&mem),
            console,
        )?;
        slots.push(VirtioSlot {
            slot: console_slot.index,
        });
    }

    Ok(DeviceLayout {
        bus: builder.build(),
        virtio_slots: slots,
        mmds,
    })
}

fn alloc_or_fail(allocator: &mut SlotAllocator) -> Result<Slot, DeviceError> {
    allocator
        .allocate()
        .map_err(|e| DeviceError::SlotExhausted(e.to_string()))
}

/// Wrap `device` in a `VirtioMmioTransport` and insert it onto the
/// builder at the slot's MMIO base.
fn plug_virtio<D: VirtioDevice + 'static>(
    builder: &mut BusBuilder,
    slot: Slot,
    gic: Arc<dyn Gic + Send + Sync>,
    mem: Arc<dyn GuestMemory>,
    device: D,
) -> Result<(), DeviceError> {
    let device_arc: Arc<Mutex<dyn VirtioDevice>> = Arc::new(Mutex::new(device));
    let irq = IrqLine::new(gic, slot.intid);
    let transport = VirtioMmioTransport::new(device_arc, mem, irq);
    let dev: Arc<Mutex<dyn BusDevice>> = Arc::new(Mutex::new(transport));
    builder.insert(dev, slot.base, squib_virtio::VIRTIO_MMIO_REGION_BYTES)?;
    Ok(())
}

/// Default guest MAC for the reference VM. Locally-administered (bit 1
/// of the first octet set), unicast (bit 0 clear). Stable across boots
/// so a guest user can pin a static lease in their host.
pub const fn default_guest_mac() -> [u8; 6] {
    [0x06, 0x00, 0xAC, 0x10, 0x00, 0x02]
}

impl NetSpec {
    /// Loopback / no-op host backend. Used in unit tests and as the safe
    /// fallback when no operator-supplied network mode is set.
    ///
    /// # Errors
    /// Surfaces [`squib_core::IdentifierError`] when `host_dev_name` fails the
    /// `HostDevName` boundary validation (empty / overlong / NUL byte).
    pub fn loopback(
        iface_id: impl Into<String>,
        host_dev_name: impl Into<String>,
    ) -> Result<Self, squib_core::IdentifierError> {
        Ok(Self {
            iface_id: iface_id.into(),
            host_dev_name: HostDevName::new(host_dev_name.into())?,
            guest_mac: None,
            mtu: None,
            backend: NetHostBackend::Loopback(LoopbackHostBackend::default()),
        })
    }
}

#[cfg(test)]
mod tests {
    use squib_arch::IntId;
    use squib_core::{GuestAddress, SliceGuestMemory};
    use squib_gic::GicError;

    use super::*;

    #[derive(Debug, Default)]
    struct StubGic;
    impl Gic for StubGic {
        fn pulse_spi(&self, _: IntId) -> Result<(), GicError> {
            Ok(())
        }
        fn set_spi_level(&self, _: IntId, _: bool) -> Result<(), GicError> {
            Ok(())
        }
        fn save_state(&self) -> Result<Vec<u8>, GicError> {
            Ok(Vec::new())
        }
        fn restore_state(&self, _: &[u8]) -> Result<(), GicError> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct DiscardSink;
    impl Pl011Sink for DiscardSink {
        fn write_byte(&mut self, _byte: u8) {}
    }

    #[test]
    fn test_should_place_pl011_at_canonical_base() {
        let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic);
        let mem: Arc<dyn GuestMemory> = Arc::new(SliceGuestMemory::new(GuestAddress(0), 0x1_0000));
        let layout = build_device_layout(
            gic,
            mem,
            DeviceBuildArgs {
                pl011_sink: Box::new(DiscardSink),
                mmds_size_cap: 8192,
                block: None,
                net: None,
                enable_console: false,
            },
        )
        .unwrap();
        // PL011 + virtio-rng = 2 devices on the bus.
        assert_eq!(layout.bus.len(), 2);
        // One virtio slot (rng).
        assert_eq!(layout.virtio_slots.len(), 1);
        assert_eq!(layout.virtio_slots[0].slot, 0);
    }

    #[test]
    fn test_should_allocate_consecutive_slots_for_block_then_net() {
        let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic);
        let mem: Arc<dyn GuestMemory> = Arc::new(SliceGuestMemory::new(GuestAddress(0), 0x1_0000));
        // Use a temp file as the block backend.
        let path = std::env::temp_dir().join("squib-devmgr-test.bin");
        std::fs::write(&path, b"hello").unwrap();
        let layout = build_device_layout(
            gic,
            mem,
            DeviceBuildArgs {
                pl011_sink: Box::new(DiscardSink),
                mmds_size_cap: 8192,
                block: Some(BlockConfigSpec {
                    id: "rootfs".into(),
                    path: path.clone(),
                    read_only: true,
                }),
                net: Some(NetSpec::loopback("eth0", "tap0").unwrap()),
                enable_console: false,
            },
        )
        .unwrap();
        // PL011 + rng + block + net = 4 bus devices, 3 virtio slots.
        assert_eq!(layout.bus.len(), 4);
        assert_eq!(layout.virtio_slots.len(), 3);
        assert_eq!(
            layout
                .virtio_slots
                .iter()
                .map(|s| s.slot)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // MMDS interceptor is a clone-shareable handle.
        assert_eq!(layout.mmds.mmds().version(), squib_mmds::MmdsVersion::V1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_should_seed_mmds_via_returned_handle() {
        let gic: Arc<dyn Gic + Send + Sync> = Arc::new(StubGic);
        let mem: Arc<dyn GuestMemory> = Arc::new(SliceGuestMemory::new(GuestAddress(0), 0x1_0000));
        let layout = build_device_layout(
            gic,
            mem,
            DeviceBuildArgs {
                pl011_sink: Box::new(DiscardSink),
                mmds_size_cap: 8192,
                block: None,
                net: Some(NetSpec::loopback("eth0", "tap0").unwrap()),
                enable_console: false,
            },
        )
        .unwrap();
        layout
            .mmds
            .mmds()
            .put_json(r#"{"latest":{"meta-data":{"instance-id":"i-1234"}}}"#)
            .unwrap();
        let v = layout
            .mmds
            .mmds()
            .get_at_pointer("/latest/meta-data/instance-id")
            .unwrap();
        assert_eq!(v, serde_json::json!("i-1234"));
    }
}
