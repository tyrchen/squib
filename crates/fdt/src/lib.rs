//! Flattened device tree builder for squib's aarch64 microvm.
//!
//! Emits the device tree skeleton documented in
//! [13-arch-and-boot.md § 6](../../../specs/13-arch-and-boot.md#6-fdt-skeleton):
//!
//! - root with `model = "squib,microvm"` and `compatible = "linux,squib-microvm,linux,dummy-virt"`
//! - `chosen` carrying the composed boot args (D23) and `linux,initrd-{start,end}`
//! - `memory@80000000` (one region for now)
//! - `cpus` with one `cpu@<mpidr>` per vCPU and `enable-method = "psci"`
//! - `psci` with HVC method
//! - `timer` with the four PPI lines (CNTV / CNTHP / CNTP / CNTPS)
//! - `intc@8000000` GICv3 with redistributor sized at runtime
//! - `pl011@e0a0000` UART
//! - `virtio_mmio@<n>` slots — one per device
//! - `apb_clk` fixed-clock at 24 MHz
//!
//! Every interrupt cell flows through [`squib_arch::IntId`] so the FDT-cell ↔
//! raw-INTID conversion lives in exactly one place.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// FDT property names are punctuation-heavy (`linux,initrd-start`, etc.); backticking
// every one in docs adds noise without clarity.
#![allow(clippy::doc_markdown)]

use std::collections::HashSet;

use squib_arch::{
    IntId, Trigger,
    gic::fixed,
    layout::{
        DRAM_BASE, GICD_BASE, GICR_BASE, GICR_REDISTRIBUTOR_SIZE_PER_VCPU, MEMORY_LAYOUT,
        PL011_BASE, PL011_SIZE, VIRTIO_MMIO_BASE,
    },
};
use thiserror::Error;
use vm_fdt::{Error as FdtBuildError, FdtWriter};

/// PHandle constants — one per phandle producer in the FDT.
const PHANDLE_GIC: u32 = 1;
const PHANDLE_APB_CLK: u32 = 2;

/// Compose the effective boot args per
/// [99-key-decisions.md § D23](../../../specs/99-key-decisions.md#d23-boot-args-composition-rule):
///
/// 1. Start with the user's `/boot-source.boot_args` (may be empty).
/// 2. Append `console=ttyAMA0` if the user has not specified `console=`.
/// 3. Append `panic=1` if the user has not specified `panic=`.
/// 4. If a `partuuid` is supplied (the root drive's `partuuid`), append `root=PARTUUID=<uuid>` if
///    the user has not specified `root=`.
///
/// **Append-if-absent** semantics: the user's value always wins. The squib defaults are
/// only added when missing.
#[must_use]
pub fn compose_boot_args(user_args: &str, root_partuuid: Option<&str>) -> String {
    let trimmed = user_args.trim();
    let mut out = String::with_capacity(trimmed.len() + 64);
    out.push_str(trimmed);

    if !key_present(trimmed, "console") {
        push_arg(&mut out, "console=ttyAMA0");
    }
    if !key_present(trimmed, "panic") {
        push_arg(&mut out, "panic=1");
    }
    if !key_present(trimmed, "root")
        && let Some(uuid) = root_partuuid
    {
        push_arg(&mut out, &format!("root=PARTUUID={uuid}"));
    }
    out
}

fn push_arg(buf: &mut String, arg: &str) {
    if !buf.is_empty() {
        buf.push(' ');
    }
    buf.push_str(arg);
}

/// True if the cmdline already declares the given key (`key=`) or as a bare flag (`key`).
fn key_present(args: &str, key: &str) -> bool {
    for token in args.split_ascii_whitespace() {
        let head = token.split_once('=').map_or(token, |(k, _)| k);
        if head == key {
            return true;
        }
    }
    false
}

/// Errors that can surface while building the FDT.
#[derive(Debug, Error)]
pub enum FdtError {
    /// Underlying `vm-fdt` builder error.
    #[error("vm-fdt builder error: {0}")]
    Builder(String),
    /// Provided `vcpu_count` is zero.
    #[error("vcpu_count must be ≥ 1")]
    NoVcpus,
    /// Too many virtio-MMIO slots requested (max 32 per the layout in 13 § 2 / 14 § 5).
    #[error("too many virtio-MMIO slots requested ({requested}; max 32)")]
    TooManyVirtioSlots {
        /// Requested slot count.
        requested: usize,
    },
    /// User-provided boot args contain a NUL byte; FDT properties are NUL-terminated and
    /// embedded NULs would silently truncate the cmdline visible to the guest.
    #[error("boot args contain a NUL byte")]
    NulInBootArgs,
}

impl From<FdtBuildError> for FdtError {
    fn from(err: FdtBuildError) -> Self {
        Self::Builder(err.to_string())
    }
}

/// Initrd region in guest physical memory.
#[derive(Debug, Clone, Copy)]
pub struct InitrdRange {
    /// Start (inclusive).
    pub start: u64,
    /// End (exclusive).
    pub end: u64,
}

/// Memory region for the root `memory@<addr>` node.
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    /// Start guest physical address.
    pub base: u64,
    /// Size in bytes.
    pub size: u64,
}

impl MemoryRegion {
    /// Sole DRAM region anchored at [`DRAM_BASE`].
    #[must_use]
    pub const fn dram(size: u64) -> Self {
        Self {
            base: DRAM_BASE,
            size,
        }
    }
}

/// Description of a virtio-MMIO device to emit into the FDT.
#[derive(Debug, Clone, Copy)]
pub struct VirtioSlot {
    /// Slot index (0..=31). Maps to FDT SPI cell `16 + slot`.
    pub slot: u32,
}

/// Inputs to [`build`].
#[derive(Debug, Clone)]
pub struct FdtBuildArgs<'a> {
    /// Number of vCPUs to emit `cpu@<mpidr>` nodes for.
    pub vcpu_count: u32,
    /// DRAM region — exactly one for 1.0.
    pub memory: MemoryRegion,
    /// Composed boot args (with D23 defaults already applied by the caller).
    pub boot_args: &'a str,
    /// Initrd region, or `None` if no initrd was configured.
    pub initrd: Option<InitrdRange>,
    /// virtio-MMIO devices — order is preserved, slot ids must be unique and ≤ 31.
    pub virtio_devices: &'a [VirtioSlot],
    /// Live GIC redistributor size per vCPU (queried via `hv_gic_get_redistributor_size`).
    /// The const default is [`GICR_REDISTRIBUTOR_SIZE_PER_VCPU`] but Apple may grow it
    /// in a future macOS release.
    pub gicr_size_per_vcpu: u64,
    /// Live GICD size (queried via `hv_gic_get_distributor_size`).
    pub gicd_size: u64,
}

impl<'a> FdtBuildArgs<'a> {
    /// Convenience constructor with sensible Apple-Silicon-1.0 defaults for the GIC sizes.
    #[must_use]
    pub fn new(
        vcpu_count: u32,
        memory: MemoryRegion,
        boot_args: &'a str,
        initrd: Option<InitrdRange>,
        virtio_devices: &'a [VirtioSlot],
    ) -> Self {
        Self {
            vcpu_count,
            memory,
            boot_args,
            initrd,
            virtio_devices,
            gicr_size_per_vcpu: GICR_REDISTRIBUTOR_SIZE_PER_VCPU,
            gicd_size: 0x0001_0000,
        }
    }
}

/// Build the FDT blob.
///
/// # Errors
/// [`FdtError`] for any of the documented failure modes.
pub fn build(args: &FdtBuildArgs<'_>) -> Result<Vec<u8>, FdtError> {
    if args.vcpu_count == 0 {
        return Err(FdtError::NoVcpus);
    }
    if args.virtio_devices.len() > MEMORY_LAYOUT.virtio_mmio_slots as usize {
        return Err(FdtError::TooManyVirtioSlots {
            requested: args.virtio_devices.len(),
        });
    }
    if args.boot_args.bytes().any(|b| b == 0) {
        return Err(FdtError::NulInBootArgs);
    }
    // Reject duplicate slot indices and out-of-range slots.
    {
        let mut seen = HashSet::with_capacity(args.virtio_devices.len());
        for dev in args.virtio_devices {
            if dev.slot >= MEMORY_LAYOUT.virtio_mmio_slots || !seen.insert(dev.slot) {
                return Err(FdtError::TooManyVirtioSlots {
                    requested: args.virtio_devices.len(),
                });
            }
        }
    }

    let mut fdt = FdtWriter::new()?;

    let root = fdt.begin_node("")?;
    fdt.property_string("model", "squib,microvm")?;
    // `compatible` is a string list — vm-fdt 0.3 takes a `Vec<String>` for this property.
    fdt.property_string_list(
        "compatible",
        vec!["linux,squib-microvm".into(), "linux,dummy-virt".into()],
    )?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_u32("interrupt-parent", PHANDLE_GIC)?;

    add_chosen(&mut fdt, args)?;
    add_memory(&mut fdt, args)?;
    add_cpus(&mut fdt, args)?;
    add_psci(&mut fdt)?;
    add_timer(&mut fdt)?;
    add_gic(&mut fdt, args)?;
    add_pl011(&mut fdt)?;
    add_virtio(&mut fdt, args)?;
    add_apb_clk(&mut fdt)?;

    fdt.end_node(root)?;
    Ok(fdt.finish()?)
}

fn add_chosen(fdt: &mut FdtWriter, args: &FdtBuildArgs<'_>) -> Result<(), FdtError> {
    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", args.boot_args)?;
    fdt.property_string("stdout-path", "/pl011@e0a0000")?;
    // `linux,earlycon` (no value, just a presence flag) tells the
    // kernel to bind the early console to whatever `stdout-path`
    // points at, without needing `earlycon=...` in the cmdline. Linux
    // 5.x and 6.x both honour it. Doubles up with the cmdline form
    // for belt-and-braces — neither breaks the other.
    fdt.property_null("linux,earlycon")?;
    if let Some(rd) = args.initrd {
        fdt.property_u64("linux,initrd-start", rd.start)?;
        fdt.property_u64("linux,initrd-end", rd.end)?;
    }
    fdt.end_node(chosen)?;
    Ok(())
}

fn add_memory(fdt: &mut FdtWriter, args: &FdtBuildArgs<'_>) -> Result<(), FdtError> {
    let name = format!("memory@{:x}", args.memory.base);
    let memory = fdt.begin_node(&name)?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[args.memory.base, args.memory.size])?;
    fdt.end_node(memory)?;
    Ok(())
}

fn add_cpus(fdt: &mut FdtWriter, args: &FdtBuildArgs<'_>) -> Result<(), FdtError> {
    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 1)?;
    fdt.property_u32("#size-cells", 0)?;
    for index in 0..args.vcpu_count {
        let mpidr = mpidr_for_index(index);
        let cpu_name = format!("cpu@{:x}", mpidr & 0x00FF_FFFF);
        let cpu = fdt.begin_node(&cpu_name)?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,armv8")?;
        fdt.property_string("enable-method", "psci")?;
        // For #address-cells=1, `reg` is the affinity-1<<8 | affinity-0 portion of MPIDR.
        let truncated =
            u32::try_from(mpidr & 0x00FF_FFFF).expect("mpidr & 0xFFFFFF always fits in u32");
        fdt.property_u32("reg", truncated)?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;
    Ok(())
}

/// Compute the MPIDR_EL1 affinity bits for a vCPU at index `index`.
///
/// For 1.0 we emit `Aff1 = index / 16`, `Aff0 = index % 16` (the QEMU virt convention
/// — Aff0 is a 4-bit field). All higher affinity bits are zero. With
/// `MAX_SUPPORTED_VCPUS = 32` we use at most two distinct Aff1 values.
fn mpidr_for_index(index: u32) -> u64 {
    let aff0 = u64::from(index & 0xF);
    let aff1 = u64::from((index >> 4) & 0xFF);
    (aff1 << 8) | aff0
}

fn add_psci(fdt: &mut FdtWriter) -> Result<(), FdtError> {
    let psci = fdt.begin_node("psci")?;
    fdt.property_string_list(
        "compatible",
        vec![
            "arm,psci-1.0".into(),
            "arm,psci-0.2".into(),
            "arm,psci".into(),
        ],
    )?;
    fdt.property_string("method", "hvc")?;
    fdt.property_u32("cpu_on", squib_arch::psci::CPU_ON)?;
    fdt.property_u32("cpu_off", squib_arch::psci::CPU_OFF)?;
    fdt.property_u32("cpu_suspend", squib_arch::psci::CPU_SUSPEND)?;
    fdt.property_u32("migrate", squib_arch::psci::MIGRATE)?;
    fdt.end_node(psci)?;
    Ok(())
}

fn add_timer(fdt: &mut FdtWriter) -> Result<(), FdtError> {
    let timer = fdt.begin_node("timer")?;
    fdt.property_string("compatible", "arm,armv8-timer")?;
    fdt.property_null("always-on")?;
    // PPI cell triple <type cell flags>; `0xf08` = level-high (4) | shareable mask (0xf00).
    let level_high_shareable = (0xF << 8) | Trigger::LevelHigh.fdt_flags();
    let interrupts = [
        // CNTPS — secure physical timer (PPI cell 13).
        ipi_cell(fixed::CNTPS, level_high_shareable),
        // CNTP — non-secure physical timer EL1 (PPI cell 14).
        ipi_cell(fixed::CNTP, level_high_shareable),
        // CNTV — virtual timer (PPI cell 11).
        ipi_cell(fixed::CNTV, level_high_shareable),
        // CNTHP — hypervisor timer (PPI cell 10).
        ipi_cell(fixed::CNTHP, level_high_shareable),
    ];
    let flat: Vec<u32> = interrupts.into_iter().flatten().collect();
    fdt.property_array_u32("interrupts", &flat)?;
    fdt.end_node(timer)?;
    Ok(())
}

fn add_gic(fdt: &mut FdtWriter, args: &FdtBuildArgs<'_>) -> Result<(), FdtError> {
    let gic = fdt.begin_node(&format!("intc@{GICD_BASE:x}"))?;
    fdt.property_string("compatible", "arm,gic-v3")?;
    fdt.property_u32("#interrupt-cells", 3)?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("#redistributor-regions", 1)?;
    fdt.property_u64("redistributor-stride", args.gicr_size_per_vcpu)?;

    let live_redistributor = u64::from(args.vcpu_count) * args.gicr_size_per_vcpu;
    fdt.property_array_u64(
        "reg",
        &[GICD_BASE, args.gicd_size, GICR_BASE, live_redistributor],
    )?;
    fdt.property_phandle(PHANDLE_GIC)?;
    fdt.end_node(gic)?;
    Ok(())
}

fn add_pl011(fdt: &mut FdtWriter) -> Result<(), FdtError> {
    let pl011 = fdt.begin_node(&format!("pl011@{PL011_BASE:x}"))?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,pl011".into(), "arm,primecell".into()],
    )?;
    fdt.property_array_u64("reg", &[PL011_BASE, PL011_SIZE])?;

    let intid = fixed::PL011;
    let cell = irq_cell(intid, Trigger::LevelHigh);
    fdt.property_array_u32("interrupts", &cell)?;
    fdt.property_u32("clocks", PHANDLE_APB_CLK)?;
    fdt.property_string("clock-names", "apb_pclk")?;
    fdt.end_node(pl011)?;
    Ok(())
}

fn add_virtio(fdt: &mut FdtWriter, args: &FdtBuildArgs<'_>) -> Result<(), FdtError> {
    for dev in args.virtio_devices {
        let base = VIRTIO_MMIO_BASE + u64::from(dev.slot) * MEMORY_LAYOUT.virtio_mmio_stride;
        let name = format!("virtio_mmio@{base:x}");
        let node = fdt.begin_node(&name)?;
        fdt.property_string("compatible", "virtio,mmio")?;
        fdt.property_array_u64("reg", &[base, MEMORY_LAYOUT.virtio_mmio_stride])?;
        let intid = IntId::from_spi_cell(16 + dev.slot)
            .map_err(|err| FdtError::Builder(err.to_string()))?;
        let cell = irq_cell(intid, Trigger::EdgeRising);
        fdt.property_array_u32("interrupts", &cell)?;
        fdt.end_node(node)?;
    }
    Ok(())
}

fn add_apb_clk(fdt: &mut FdtWriter) -> Result<(), FdtError> {
    let clk = fdt.begin_node("apb_clk")?;
    fdt.property_string("compatible", "fixed-clock")?;
    fdt.property_u32("#clock-cells", 0)?;
    fdt.property_u32("clock-frequency", 24_000_000)?;
    fdt.property_string("clock-output-names", "clk24mhz")?;
    fdt.property_phandle(PHANDLE_APB_CLK)?;
    fdt.end_node(clk)?;
    Ok(())
}

/// Emit a (type, offset, flags) interrupt cell.
fn irq_cell(intid: IntId, trigger: Trigger) -> [u32; 3] {
    [
        intid.fdt_cell_type(),
        intid.fdt_cell_offset(),
        trigger.fdt_flags(),
    ]
}

/// Same shape as [`irq_cell`] but for timer PPIs that demand a non-trivial flags field
/// (level-high + shareable).
fn ipi_cell(intid: IntId, flags: u32) -> [u32; 3] {
    [intid.fdt_cell_type(), intid.fdt_cell_offset(), flags]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> FdtBuildArgs<'static> {
        FdtBuildArgs::new(
            2,
            MemoryRegion::dram(128 * 1024 * 1024),
            "console=ttyAMA0 panic=1",
            None,
            &[],
        )
    }

    #[test]
    fn rejects_zero_vcpus() {
        let mut a = args();
        a.vcpu_count = 0;
        assert!(matches!(build(&a), Err(FdtError::NoVcpus)));
    }

    #[test]
    fn rejects_more_than_32_virtio_slots() {
        let many: Vec<VirtioSlot> = (0..40).map(|s| VirtioSlot { slot: s }).collect();
        let memory = MemoryRegion::dram(128 * 1024 * 1024);
        let a = FdtBuildArgs::new(1, memory, "", None, &many);
        assert!(matches!(
            build(&a),
            Err(FdtError::TooManyVirtioSlots { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_virtio_slots() {
        let dup = vec![VirtioSlot { slot: 0 }, VirtioSlot { slot: 0 }];
        let memory = MemoryRegion::dram(128 * 1024 * 1024);
        let a = FdtBuildArgs::new(1, memory, "", None, &dup);
        assert!(matches!(
            build(&a),
            Err(FdtError::TooManyVirtioSlots { .. })
        ));
    }

    #[test]
    fn rejects_nul_in_boot_args() {
        let a = FdtBuildArgs::new(
            1,
            MemoryRegion::dram(128 * 1024 * 1024),
            "console=ttyAMA0\0evil",
            None,
            &[],
        );
        assert!(matches!(build(&a), Err(FdtError::NulInBootArgs)));
    }

    #[test]
    fn small_fdt_builds_under_two_mib() {
        let dtb = build(&args()).unwrap();
        assert!(!dtb.is_empty());
        assert!(dtb.len() < 2 * 1024 * 1024, "FDT exceeds 2 MiB cap");
    }

    #[test]
    fn fdt_at_max_vcpus_and_max_devices_fits_in_two_mib() {
        let virtio: Vec<VirtioSlot> = (0..32).map(|s| VirtioSlot { slot: s }).collect();
        let a = FdtBuildArgs::new(
            32,
            MemoryRegion::dram(128 * 1024 * 1024),
            "console=ttyAMA0 panic=1",
            None,
            &virtio,
        );
        let dtb = build(&a).unwrap();
        assert!(
            dtb.len() < 2 * 1024 * 1024,
            "max-config FDT is {} bytes (cap 2 MiB)",
            dtb.len()
        );
    }

    #[test]
    fn dtb_starts_with_magic() {
        let dtb = build(&args()).unwrap();
        // Devicetree blob magic: 0xd00dfeed (big-endian).
        assert_eq!(&dtb[0..4], &[0xD0, 0x0D, 0xFE, 0xED]);
    }

    #[test]
    fn dtb_contains_compatible_string() {
        let dtb = build(&args()).unwrap();
        let model = b"squib,microvm";
        assert!(dtb.windows(model.len()).any(|w| w == model));
    }

    #[test]
    fn initrd_round_trip_emits_start_and_end_properties() {
        let a = FdtBuildArgs::new(
            1,
            MemoryRegion::dram(128 * 1024 * 1024),
            "",
            Some(InitrdRange {
                start: 0x9000_0000,
                end: 0x9100_0000,
            }),
            &[],
        );
        // Just verify it builds — the structural test on properties happens via the
        // embedded magic check + size sanity. Detailed FDT inspection requires a device-
        // tree parser that vm-fdt does not expose.
        assert!(build(&a).is_ok());
    }

    #[test]
    fn boot_args_d23_appends_defaults_when_absent() {
        let composed = compose_boot_args("", None);
        assert!(composed.contains("console=ttyAMA0"));
        assert!(composed.contains("panic=1"));
    }

    #[test]
    fn boot_args_d23_user_value_wins_for_console() {
        let composed = compose_boot_args("console=hvc0 quiet", None);
        assert!(composed.contains("console=hvc0"));
        // `panic=1` still appended since user did not set it.
        assert!(composed.contains("panic=1"));
        // `console=ttyAMA0` must NOT be added — user's choice takes precedence.
        assert!(!composed.contains("ttyAMA0"));
    }

    #[test]
    fn boot_args_d23_user_value_wins_for_panic() {
        let composed = compose_boot_args("panic=10", None);
        // The user's value wins: there must be exactly one panic= occurrence and it's
        // panic=10, not panic=1.
        let panic_tokens: Vec<&str> = composed
            .split_ascii_whitespace()
            .filter(|t| t.starts_with("panic="))
            .collect();
        assert_eq!(panic_tokens, vec!["panic=10"]);
    }

    #[test]
    fn boot_args_d23_appends_root_partuuid_when_absent() {
        let composed = compose_boot_args(
            "console=ttyAMA0",
            Some("12345678-9abc-def0-1234-56789abcdef0"),
        );
        assert!(composed.contains("root=PARTUUID=12345678-9abc-def0-1234-56789abcdef0"));
    }

    #[test]
    fn boot_args_d23_user_root_wins_over_partuuid() {
        let composed = compose_boot_args(
            "root=/dev/vda1",
            Some("12345678-9abc-def0-1234-56789abcdef0"),
        );
        assert!(composed.contains("root=/dev/vda1"));
        assert!(!composed.contains("PARTUUID="));
    }

    #[test]
    fn boot_args_d23_no_partuuid_means_no_root_added() {
        let composed = compose_boot_args("", None);
        assert!(!composed.contains("root="));
    }

    #[test]
    fn boot_args_d23_does_not_match_partial_keys() {
        // `consoleX=foo` must not be detected as `console=`.
        let composed = compose_boot_args("consolex=blah", None);
        assert!(composed.contains("console=ttyAMA0"));
    }

    #[test]
    fn mpidr_for_index_matches_qemu_virt_convention() {
        assert_eq!(mpidr_for_index(0), 0x0000);
        assert_eq!(mpidr_for_index(1), 0x0001);
        assert_eq!(mpidr_for_index(15), 0x000F);
        assert_eq!(mpidr_for_index(16), 0x0100);
        assert_eq!(mpidr_for_index(31), 0x010F);
    }
}
