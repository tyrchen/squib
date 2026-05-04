//! Diagnostic: dump HVF's GIC sizes/alignments. Run via `make hvf-test`.
#![cfg(target_os = "macos")]
#![allow(clippy::disallowed_methods, clippy::uninlined_format_args)]
#[test]
#[ignore = "diagnostic; sign + run via the hvf-test makefile target"]
fn print_gic_facts() {
    use applevisor::gic::GicConfig;
    println!("dist_size = {:?}", GicConfig::get_distributor_size());
    println!(
        "dist_align = {:?}",
        GicConfig::get_distributor_base_alignment()
    );
    println!(
        "redist_region_size = {:?}",
        GicConfig::get_redistributor_region_size()
    );
    println!(
        "redist_size_per_vcpu = {:?}",
        GicConfig::get_redistributor_size()
    );
    println!(
        "redist_align = {:?}",
        GicConfig::get_redistributor_base_alignment()
    );
    println!("spi_range = {:?}", GicConfig::get_spi_interrupt_range());
}
