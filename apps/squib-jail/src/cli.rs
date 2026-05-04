//! `squib-jail` CLI flag definitions, mirroring upstream Firecracker `jailer`.
//!
//! Every flag from upstream parses without error (I-JAIL-1). Flags whose
//! semantics do not exist on Darwin (`--cgroup`, `--parent-cgroup`,
//! `--cgroup-version`, `--netns`, `--new-pid-ns`) are accepted and surfaced via
//! [`Args::warn_unsupported`] so the operator sees a single, predictable
//! warning per launch. See `specs/40-jailer.md` § 2.

use std::path::PathBuf;

use clap::{Parser, ValueEnum, builder::TypedValueParser};
use tracing::warn;

/// Byte cap for short, identity-shaped flags. The chroot id, sandbox profile name,
/// cgroup version, and resource-limit values all fit comfortably under 256 B; a
/// value larger than that is by definition not a real id, just a denial-of-service
/// against any downstream parser ([70-security.md §
/// 4](../../specs/70-security.md#4-input-validation)).
const SHORT_FLAG_MAX_BYTES: usize = 256;
/// Byte cap for path-shaped flags. macOS `PATH_MAX` is 1024 B; we accept up to that
/// for `--exec-file`, `--chroot-base-dir`, `--netns`, etc. so a real-world deep
/// chroot tree still passes while a megabyte-of-junk argv bombs out at parse time.
const PATH_FLAG_MAX_BYTES: usize = 1024;
/// Byte cap for child argv (after `--`). Squib's own CLI accepts no flag near
/// this limit (the longest is `--config-file` followed by a path), so 4 KiB
/// per element is a comfortable ceiling that still defeats argv-pumping `DoS`.
const CHILD_ARG_MAX_BYTES: usize = 4096;
/// Byte cap on the *sum* of child argv element lengths — bounds total argv area
/// so an operator cannot trickle 1024 elements of 4 KiB each through.
const CHILD_ARG_TOTAL_MAX_BYTES: usize = 16 * 1024;

fn capped_string_parser(max_bytes: usize) -> impl TypedValueParser<Value = String> {
    clap::builder::StringValueParser::new().try_map(move |s: String| -> Result<String, String> {
        if s.len() > max_bytes {
            return Err(format!("value is {} bytes; max {max_bytes}", s.len()));
        }
        if s.as_bytes().contains(&0) {
            return Err("value contains an interior NUL byte".to_string());
        }
        Ok(s)
    })
}

fn capped_path_parser() -> impl TypedValueParser<Value = PathBuf> {
    clap::builder::StringValueParser::new().try_map(|s: String| -> Result<PathBuf, String> {
        if s.len() > PATH_FLAG_MAX_BYTES {
            return Err(format!(
                "path is {} bytes; max {PATH_FLAG_MAX_BYTES}",
                s.len()
            ));
        }
        if s.as_bytes().contains(&0) {
            return Err("path contains an interior NUL byte".to_string());
        }
        Ok(PathBuf::from(s))
    })
}

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
    #[arg(long, value_name = "ID", value_parser = capped_string_parser(SHORT_FLAG_MAX_BYTES))]
    pub(crate) id: String,

    /// Path to the binary to exec into (the squib binary, by convention).
    #[arg(long, value_name = "PATH", value_parser = capped_path_parser())]
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
        env = "SQUIB_JAIL_BASE",
        value_parser = capped_path_parser(),
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
    #[arg(long = "resource-limit", value_name = "K=V", value_parser = capped_string_parser(SHORT_FLAG_MAX_BYTES))]
    pub(crate) resource_limits: Vec<String>,

    /// Linux cgroup file=value pair. Accepted-and-warned on Darwin.
    #[arg(long = "cgroup", value_name = "K=V", value_parser = capped_string_parser(SHORT_FLAG_MAX_BYTES))]
    pub(crate) cgroups: Vec<String>,

    /// Linux parent cgroup. Accepted-and-warned on Darwin.
    #[arg(long, value_name = "PATH", value_parser = capped_string_parser(PATH_FLAG_MAX_BYTES))]
    pub(crate) parent_cgroup: Option<String>,

    /// Linux cgroup version (1 or 2). Accepted-and-warned on Darwin.
    #[arg(long, value_name = "VERSION", default_value = "1", value_parser = capped_string_parser(SHORT_FLAG_MAX_BYTES))]
    pub(crate) cgroup_version: String,

    /// Linux netns path. Accepted-and-warned on Darwin.
    #[arg(long, value_name = "PATH", value_parser = capped_path_parser())]
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
    #[arg(last = true, value_parser = capped_string_parser(CHILD_ARG_MAX_BYTES))]
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
    /// Defence-in-depth check on the *sum* of child argv element lengths.
    /// clap's per-element `value_parser` already caps each value, but a
    /// thousand-element argv summing to a megabyte still passes that. Bound the
    /// total here so the wire shape is provably small.
    pub(crate) fn validate_passthrough_argv_total(&self) -> Result<(), String> {
        let total: usize = self.passthrough_argv.iter().map(String::len).sum();
        if total > CHILD_ARG_TOTAL_MAX_BYTES {
            return Err(format!(
                "passthrough argv totals {total} bytes; max {CHILD_ARG_TOTAL_MAX_BYTES}"
            ));
        }
        Ok(())
    }

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
    fn cli_rejects_overlong_id() {
        let huge = "a".repeat(SHORT_FLAG_MAX_BYTES + 1);
        let err = Args::try_parse_from([
            "squib-jail",
            "--id",
            &huge,
            "--exec-file",
            "/usr/local/bin/squib",
            "--uid",
            "0",
            "--gid",
            "0",
        ])
        .unwrap_err();
        assert!(err.to_string().contains("max"));
    }

    #[test]
    fn cli_rejects_nul_in_path() {
        let err = Args::try_parse_from([
            "squib-jail",
            "--id",
            "vm1",
            "--exec-file",
            "/bad\0path",
            "--uid",
            "0",
            "--gid",
            "0",
        ])
        .unwrap_err();
        assert!(err.to_string().contains("NUL"));
    }

    #[test]
    fn cli_validates_passthrough_argv_total() {
        let mut argv = vec![
            "squib-jail".to_string(),
            "--id".to_string(),
            "vm1".to_string(),
            "--exec-file".to_string(),
            "/usr/local/bin/squib".to_string(),
            "--uid".to_string(),
            "0".to_string(),
            "--gid".to_string(),
            "0".to_string(),
            "--".to_string(),
        ];
        // Each element 4 KiB → six elements push past the 16 KiB total cap.
        for _ in 0..6 {
            argv.push("a".repeat(CHILD_ARG_MAX_BYTES - 1));
        }
        let parsed = Args::try_parse_from(&argv).expect("per-element cap allows 4 KiB args");
        let err = parsed.validate_passthrough_argv_total().unwrap_err();
        assert!(err.contains("max"));
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
