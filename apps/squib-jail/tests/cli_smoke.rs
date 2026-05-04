//! Integration tests for `squib-jail` covering the parts of the I-JAIL-*
//! invariants that are observable without root.
//!
//! These smoke tests spawn the squib-jail binary directly and read its
//! exit code. The workspace's `clippy.toml` maps `std::process::Command`
//! to its `tokio::process::Command` async counterpart; that mapping is
//! correct for the rest of the workspace and incorrect for these tests
//! which intentionally do not pull in a tokio runtime. We allow the lint
//! at file scope and document the reason inline.

#![allow(clippy::disallowed_types)]
//!
//! - **I-JAIL-1** (every upstream `jailer` flag parses without error): we run the binary with the
//!   full upstream flag set against a non-existent `--exec-file` and assert the failure mode is
//!   "exec-file does not canonicalize", not "argument parser rejected the flags". That cleanly
//!   separates the parse layer from the env-validation layer.
//! - **I-JAIL-2** (exit codes match upstream): we assert the binary exits with the documented
//!   numeric code (1 for parse / validation, 2 for privilege failures) on a small set of
//!   representative inputs.
//! - **I-JAIL-4** (sandbox profile failures surface as a non-zero exit): covered by a unit test in
//!   `sandbox.rs`; the live `sandbox_init` call is only meaningful on macOS at runtime, so the
//!   smoke is opt-in via `--include-ignored`.

use std::process::Command;

/// Path to the test-built binary. Cargo sets `CARGO_BIN_EXE_<name>` for
/// every `[[bin]]` target in the package.
fn squib_jail_bin() -> &'static str {
    env!("CARGO_BIN_EXE_squib-jail")
}

#[test]
fn parses_full_upstream_flag_set_then_fails_on_missing_exec_file() {
    // `--exec-file /nonexistent/squib` will canonicalize-fail (exit 1)
    // *after* the full upstream flag set has parsed.
    let out = Command::new(squib_jail_bin())
        .args([
            "--id",
            "vm1",
            "--exec-file",
            "/this/path/does/not/exist/squib",
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
            "--resource-limit",
            "no-file=1024",
            "--cgroup-version",
            "2",
            "--parent-cgroup",
            "fc.slice",
        ])
        .output()
        .expect("spawn squib-jail");

    let code = out.status.code().expect("not killed by signal");
    assert_eq!(code, 1, "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("canonicalize") || stderr.contains("not a regular file"),
        "expected canonicalize failure, got: {stderr}"
    );
}

#[test]
fn rejects_invalid_id_with_exit_one() {
    // Hyphens are valid (upstream-shaped); dots and slashes are not.
    let out = Command::new(squib_jail_bin())
        .args([
            "--id",
            "vm.with.dots",
            "--exec-file",
            "/usr/bin/true",
            "--uid",
            "0",
            "--gid",
            "0",
        ])
        .output()
        .expect("spawn squib-jail");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid instance id"), "stderr: {stderr}");
}

#[test]
fn rejects_invalid_resource_limit_with_exit_one() {
    let out = Command::new(squib_jail_bin())
        .args([
            "--id",
            "vm1",
            "--exec-file",
            "/usr/bin/true",
            "--uid",
            "0",
            "--gid",
            "0",
            "--resource-limit",
            "rss=128",
        ])
        .output()
        .expect("spawn squib-jail");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("rss"), "stderr: {stderr}");
}

#[test]
fn rejects_unknown_sandbox_profile_at_clap_layer() {
    // ValueEnum is enforced by clap, so this should exit non-zero
    // with a clap-shaped usage error before main runs.
    let out = Command::new(squib_jail_bin())
        .args([
            "--id",
            "vm1",
            "--exec-file",
            "/usr/bin/true",
            "--uid",
            "0",
            "--gid",
            "0",
            "--macos-sandbox-profile",
            "evil",
        ])
        .output()
        .expect("spawn squib-jail");
    let code = out.status.code().expect("not killed by signal");
    assert!(code != 0, "expected non-zero exit, got {code}");
}

#[test]
fn rejects_missing_required_flags() {
    let out = Command::new(squib_jail_bin())
        .args(["--id", "vm1"]) // missing --exec-file / --uid / --gid
        .output()
        .expect("spawn squib-jail");
    assert!(out.status.code().is_some());
    assert_ne!(out.status.code(), Some(0));
}

#[test]
fn version_flag_prints_and_exits_zero() {
    let out = Command::new(squib_jail_bin())
        .arg("--version")
        .output()
        .expect("spawn squib-jail");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("squib-jail"), "stdout: {stdout}");
}
