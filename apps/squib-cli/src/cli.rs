//! CLI flag definitions, mirroring the Firecracker CLI documented in
//! `specs/squib-api-compat-design.md`.
//!
//! Linux-only flags (e.g. `--seccomp-filter`, `--enable-pci`) are accepted for compatibility
//! and surfaced to `main.rs` so it can emit the documented one-time warnings.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Squib's CLI surface. Every flag is either a Firecracker passthrough or a documented
/// squib-only extension (prefixed in help text accordingly).
#[derive(Debug, Parser)]
#[command(
    name = "squib",
    version,
    about = "macOS-native microVM monitor with a Firecracker-compatible API",
    long_about = None,
)]
#[allow(clippy::struct_excessive_bools)] // each bool maps to one Firecracker-compat CLI flag
pub(crate) struct Args {
    /// Path to the Unix domain socket the API server binds (Firecracker default).
    #[arg(long, value_name = "PATH", default_value = "/run/firecracker.socket")]
    pub(crate) api_sock: Option<PathBuf>,

    /// `MicroVM` instance identifier. Must match `^[A-Za-z0-9_]+$`.
    #[arg(long, value_name = "ID", default_value = "anonymous")]
    pub(crate) id: String,

    /// JSON configuration file consumed at boot in lieu of API calls.
    #[arg(long, value_name = "PATH")]
    pub(crate) config_file: Option<PathBuf>,

    /// JSON file used to seed the MMDS data store.
    #[arg(long, value_name = "PATH")]
    pub(crate) metadata: Option<PathBuf>,

    /// Run without an API socket (requires `--config-file`).
    #[arg(long)]
    pub(crate) no_api: bool,

    /// Path to a custom seccomp filter (accepted for compatibility — no-op on macOS).
    #[arg(long, value_name = "PATH", conflicts_with = "no_seccomp")]
    pub(crate) seccomp_filter: Option<PathBuf>,

    /// Disable seccomp filtering (accepted for compatibility — no-op on macOS).
    #[arg(long, conflicts_with = "seccomp_filter")]
    pub(crate) no_seccomp: bool,

    /// Path to a regular file or named pipe for log output.
    #[arg(long, value_name = "PATH")]
    pub(crate) log_path: Option<PathBuf>,

    /// Log level.
    #[arg(long, value_enum, default_value_t = LogLevel::Info)]
    pub(crate) level: LogLevel,

    /// Tracing module filter.
    #[arg(long, value_name = "MODULE")]
    pub(crate) module: Option<String>,

    /// Include the level in each log line.
    #[arg(long)]
    pub(crate) show_level: bool,

    /// Include the `file:line` origin in each log line.
    #[arg(long)]
    pub(crate) show_log_origin: bool,

    /// Path to a regular file or named pipe for metrics output.
    #[arg(long, value_name = "PATH")]
    pub(crate) metrics_path: Option<PathBuf>,

    /// Maximum HTTP request body size (Firecracker default 51200).
    #[arg(long, value_name = "BYTES", default_value_t = 51_200)]
    pub(crate) http_api_max_payload_size: u32,

    /// Maximum size of the MMDS data store, in bytes.
    #[arg(long, value_name = "BYTES")]
    pub(crate) mmds_size_limit: Option<u32>,

    /// Enable the boot timer device.
    #[arg(long)]
    pub(crate) boot_timer: bool,

    /// Enable virtio-PCI transport (accepted for compatibility — squib uses MMIO).
    #[arg(long)]
    pub(crate) enable_pci: bool,

    /// Wall-clock microseconds at process start (set by jailer).
    #[arg(long, value_name = "MICROS")]
    pub(crate) start_time_us: Option<u64>,

    /// CPU microseconds at process start (set by jailer).
    #[arg(long, value_name = "MICROS")]
    pub(crate) start_time_cpu_us: Option<u64>,

    /// CPU microseconds spent in the parent process (set by jailer).
    #[arg(long, value_name = "MICROS")]
    pub(crate) parent_cpu_time_us: Option<u64>,

    /// Print the snapshot format version supported by this build.
    #[arg(long)]
    pub(crate) snapshot_version: bool,

    /// Print the format version embedded in the snapshot at PATH.
    #[arg(long, value_name = "PATH")]
    pub(crate) describe_snapshot: Option<PathBuf>,

    /// Networking mode (squib extension).
    #[arg(long, value_enum, default_value_t = NetworkMode::Shared)]
    pub(crate) network: NetworkMode,

    /// Path to the bundled `gvproxy` binary. Required for `--network=userspace`.
    /// Falls back to `SQUIB_GVPROXY_PATH` and then `/usr/local/libexec/squib/gvproxy`.
    #[arg(long, value_name = "PATH", env = "SQUIB_GVPROXY_PATH")]
    pub(crate) gvproxy_path: Option<PathBuf>,

    /// Host-side physical interface to bridge against when `--network=bridged` is in use
    /// (e.g. `en0`, `en1`). When omitted, vmnet picks the primary interface — fine for
    /// the inner-dev-loop default but operators with multi-NIC hosts need this knob to
    /// disambiguate. Has no effect outside `--network=bridged`.
    #[arg(long, value_name = "IFNAME")]
    pub(crate) bridged_iface: Option<String>,
}

/// Firecracker-compatible log level set.
#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "PascalCase")]
pub(crate) enum LogLevel {
    /// Suppress all logging.
    Off,
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warning,
    /// Informational events.
    Info,
    /// Per-request debug events.
    Debug,
    /// Verbose tracing events.
    Trace,
}

impl LogLevel {
    pub(crate) fn as_directive(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warning => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// Host networking mode.
#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum NetworkMode {
    /// vmnet-shared (NAT). Default. No entitlement beyond `com.apple.security.hypervisor`.
    Shared,
    /// vmnet-bridged. Requires the restricted form of `com.apple.vm.networking`.
    /// Only available when squib is built with the `bridged` cargo feature.
    Bridged,
    /// vmnet-host (host-only network). No entitlement beyond `com.apple.security.hypervisor`.
    Host,
    /// Embedded gvproxy userspace stack. No entitlement required.
    Userspace,
}

impl NetworkMode {
    /// Translate the CLI choice into the [`squib_net::NetMode`] that the device
    /// manager consumes. `Bridged` is only supported when squib is built with
    /// the `bridged` cargo feature; otherwise an error is returned and the
    /// caller should `bail!` on the message.
    //
    // The Result wrap is conditional on the `bridged` feature; clippy's
    // `unnecessary_wraps` only fires in the bridged build because every arm
    // returns `Ok`. The default (non-bridged) build's `Bridged` arm
    // legitimately returns `Err`, so the Result is required there.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn to_net_mode(self) -> Result<squib_net::NetMode, &'static str> {
        match self {
            Self::Shared => Ok(squib_net::NetMode::SHARED),
            Self::Host => Ok(squib_net::NetMode::HOST),
            Self::Userspace => Ok(squib_net::NetMode::USERSPACE),
            Self::Bridged => {
                #[cfg(feature = "bridged")]
                {
                    Ok(squib_net::NetMode::BRIDGED)
                }
                #[cfg(not(feature = "bridged"))]
                {
                    Err(
                        "--network=bridged requires a binary built with `--features bridged` and \
                         signed with `com.apple.vm.networking`. See specs/30-networking.md § 5.",
                    )
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_parses_minimal_invocation() {
        let args = Args::try_parse_from(["squib"]).unwrap();
        assert_eq!(args.id, "anonymous");
        assert!(matches!(args.network, NetworkMode::Shared));
    }

    #[test]
    fn cli_parses_full_firecracker_compat_invocation() {
        let args = Args::try_parse_from([
            "squib",
            "--api-sock",
            "/tmp/fc.sock",
            "--id",
            "vm1",
            "--config-file",
            "/tmp/vm.json",
            "--log-path",
            "/tmp/fc.log",
            "--level",
            "Debug",
            "--show-level",
            "--show-log-origin",
            "--metrics-path",
            "/tmp/fc.metrics",
            "--boot-timer",
            "--http-api-max-payload-size",
            "65536",
        ])
        .unwrap();
        assert_eq!(args.id, "vm1");
        assert_eq!(args.http_api_max_payload_size, 65_536);
        assert!(args.boot_timer);
        assert!(args.show_level);
    }

    #[test]
    fn seccomp_flags_are_mutually_exclusive() {
        let err =
            Args::try_parse_from(["squib", "--seccomp-filter", "/x", "--no-seccomp"]).unwrap_err();
        assert!(err.to_string().contains("cannot be used with"));
    }

    #[test]
    fn cli_command_definition_is_well_formed() {
        Args::command().debug_assert();
    }

    #[test]
    fn cli_should_parse_userspace_network_mode_with_gvproxy_path() {
        let args = Args::try_parse_from([
            "squib",
            "--network",
            "userspace",
            "--gvproxy-path",
            "/opt/squib/gvproxy",
        ])
        .unwrap();
        assert!(matches!(args.network, NetworkMode::Userspace));
        assert_eq!(
            args.gvproxy_path.as_deref().and_then(|p| p.to_str()),
            Some("/opt/squib/gvproxy")
        );
    }

    #[test]
    fn cli_should_translate_shared_into_squib_net_shared_mode() {
        let m = NetworkMode::Shared.to_net_mode().unwrap();
        assert!(matches!(m, squib_net::NetMode::Vmnet(_)));
        assert!(!m.needs_restricted_entitlement());
    }

    #[test]
    fn cli_should_reject_bridged_without_feature_flag() {
        // The default build does not include the `bridged` cargo feature.
        let r = NetworkMode::Bridged.to_net_mode();
        #[cfg(not(feature = "bridged"))]
        {
            assert!(r.is_err());
        }
        #[cfg(feature = "bridged")]
        {
            assert!(r.is_ok());
        }
    }
}
