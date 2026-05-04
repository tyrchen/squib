//! `squib-jail` CLI flag definitions, mirroring upstream Firecracker `jailer`.
//!
//! Every flag from upstream parses without error (I-JAIL-1). Flags whose
//! semantics do not exist on Darwin (`--cgroup`, `--parent-cgroup`,
//! `--cgroup-version`, `--netns`, `--new-pid-ns`) are accepted and surfaced via
//! [`Args::warn_unsupported`] so the operator sees a single, predictable
//! warning per launch. See `specs/40-jailer.md` § 2.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use tracing::warn;

/// Squib-jail CLI surface — kept in lockstep with upstream `jailer` so launchers
/// can swap one binary for the other.
#[derive(Debug, Parser)]
#[command(
    name = "squib-jail",
    version,
    about = "Drop-in Darwin shim for the Firecracker `jailer` binary",
    long_about = None,
)]
pub(crate) struct Args {
    /// Instance identifier; used in chroot path naming.
    #[arg(long, value_name = "ID")]
    pub(crate) id: String,

    /// Path to the binary to exec into (the squib binary, by convention).
    #[arg(long, value_name = "PATH")]
    pub(crate) exec_file: PathBuf,

    /// Numeric uid the jailer setuids to before exec.
    #[arg(long, value_name = "UID")]
    pub(crate) uid: u32,

    /// Numeric gid the jailer setgids to before exec.
    #[arg(long, value_name = "GID")]
    pub(crate) gid: u32,

    /// Base directory under which per-instance chroots are created.
    /// Default `/srv/jailer` matches upstream; override via `SQUIB_JAIL_BASE`.
    #[arg(
        long,
        value_name = "PATH",
        default_value = "/srv/jailer",
        env = "SQUIB_JAIL_BASE"
    )]
    pub(crate) chroot_base_dir: PathBuf,

    /// `setsid(2)` + redirect stdio onto `/dev/null` before exec. Genuine on
    /// Darwin (I-JAIL-2 covers the exit code on failure).
    #[arg(long)]
    pub(crate) daemonize: bool,

    /// Linux PID-namespace flag. No PID namespaces on Darwin; squib-jail
    /// emits a one-time warning and otherwise no-ops. The signal-lineage
    /// effect on Linux (a fresh init for the spawned process) is not
    /// reproducible without a true PID namespace.
    #[arg(long)]
    pub(crate) new_pid_ns: bool,

    /// `<resource>=<value>` pairs for `setrlimit(2)`. Repeatable. Allowed
    /// keys: `fsize`, `no-file` (matching upstream — see `specs/40-jailer.md`
    /// § 2.1).
    #[arg(long = "resource-limit", value_name = "K=V")]
    pub(crate) resource_limits: Vec<String>,

    /// Linux cgroup file=value pair. Accepted-and-warned on Darwin.
    #[arg(long = "cgroup", value_name = "K=V")]
    pub(crate) cgroups: Vec<String>,

    /// Linux parent cgroup. Accepted-and-warned on Darwin.
    #[arg(long, value_name = "PATH")]
    pub(crate) parent_cgroup: Option<String>,

    /// Linux cgroup version (1 or 2). Accepted-and-warned on Darwin.
    #[arg(long, value_name = "VERSION", default_value = "1")]
    pub(crate) cgroup_version: String,

    /// Linux netns path. Accepted-and-warned on Darwin.
    #[arg(long, value_name = "PATH")]
    pub(crate) netns: Option<PathBuf>,

    /// Apply a bundled `sandbox_init(3)` profile by name. Squib extension
    /// (see `specs/40-jailer.md` § 2.3).
    #[arg(long, value_enum, value_name = "NAME")]
    pub(crate) macos_sandbox_profile: Option<SandboxProfile>,

    /// Tracing log level for the jailer's own warnings.
    #[arg(long, value_enum, default_value_t = LogLevel::Info)]
    pub(crate) level: LogLevel,

    /// Arguments to pass through to the staged binary, separated from the
    /// jailer's own argv by `--`.
    #[arg(last = true)]
    pub(crate) passthrough_argv: Vec<String>,
}

/// Bundled `sandbox_init(3)` profile names. Matches the layout under
/// `apps/squib-jail/profiles/`.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
#[value(rename_all = "lowercase")]
pub(crate) enum SandboxProfile {
    /// Deny network egress, allow vmnet socket, allow `/srv/jailer/<id>` r/w.
    Default,
    /// Close to no sandboxing; for debugging.
    Permissive,
}

/// Tracing log level shared with `apps/squib`.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
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

impl Args {
    /// Emit one-time warnings for the Linux-only flags so an operator
    /// running squib-jail with a Linux-shaped invocation sees that the
    /// Darwin shim cannot fulfil those semantics. See
    /// `specs/40-jailer.md` § 2.2 for the matrix.
    pub(crate) fn warn_unsupported(&self) {
        if !self.cgroups.is_empty() || self.parent_cgroup.is_some() || self.cgroup_version != "1" {
            warn!(
                "--cgroup / --parent-cgroup / --cgroup-version are accepted for Firecracker \
                 compatibility but no-op on macOS"
            );
        }
        if self.netns.is_some() {
            warn!("--netns is accepted for compatibility but no-op on macOS (no Linux netns)");
        }
        if self.new_pid_ns {
            warn!(
                "--new-pid-ns is accepted for compatibility but no-op on macOS; signal lineage is \
                 decoupled via posix_spawn-equivalent fork+exec"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    /// I-JAIL-1: every upstream `jailer` flag parses without error.
    #[test]
    fn cli_parses_full_upstream_flag_set() {
        let args = Args::try_parse_from([
            "squib-jail",
            "--id",
            "vm1",
            "--exec-file",
            "/usr/local/bin/squib",
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--chroot-base-dir",
            "/srv/jailer",
            "--netns",
            "/var/run/netns/foo",
            "--daemonize",
            "--new-pid-ns",
            "--cgroup",
            "cpu.shares=10",
            "--cgroup",
            "memory.limit_in_bytes=128M",
            "--resource-limit",
            "no-file=1024",
            "--resource-limit",
            "fsize=1073741824",
            "--cgroup-version",
            "2",
            "--parent-cgroup",
            "fc.slice",
            "--",
            "--api-sock",
            "/run/firecracker.socket",
        ])
        .expect("upstream flag set must parse");
        assert_eq!(args.id, "vm1");
        assert_eq!(args.uid, 1000);
        assert_eq!(args.gid, 1000);
        assert_eq!(args.cgroups.len(), 2);
        assert_eq!(args.resource_limits.len(), 2);
        assert!(args.daemonize);
        assert!(args.new_pid_ns);
        assert_eq!(args.cgroup_version, "2");
        assert_eq!(args.parent_cgroup.as_deref(), Some("fc.slice"));
        assert_eq!(
            args.netns.as_deref().and_then(|p| p.to_str()),
            Some("/var/run/netns/foo")
        );
        assert_eq!(
            args.passthrough_argv,
            vec!["--api-sock", "/run/firecracker.socket"]
        );
    }

    #[test]
    fn cli_parses_squib_extension() {
        let args = Args::try_parse_from([
            "squib-jail",
            "--id",
            "vm1",
            "--exec-file",
            "/usr/local/bin/squib",
            "--uid",
            "501",
            "--gid",
            "20",
            "--macos-sandbox-profile",
            "permissive",
        ])
        .unwrap();
        assert_eq!(args.macos_sandbox_profile, Some(SandboxProfile::Permissive));
    }

    #[test]
    fn cli_command_definition_is_well_formed() {
        Args::command().debug_assert();
    }

    #[test]
    fn cli_rejects_missing_required_flags() {
        let err = Args::try_parse_from(["squib-jail", "--id", "vm1"]).unwrap_err();
        // clap surfaces the missing `--exec-file` / `--uid` / `--gid`
        // through one or more "required arguments were not provided"
        // messages — the exact wording is not load-bearing.
        let s = err.to_string();
        assert!(
            s.contains("required") || s.contains("not provided"),
            "unexpected: {s}"
        );
    }
}
