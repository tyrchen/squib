//! `squib-jail` — Darwin shim for the Firecracker `jailer` binary.
//!
//! Runs the upstream-jailer flag set, applies the safe Unix subset
//! (`chroot(2)` + `setuid(2)` + `setgid(2)` + `setrlimit(2)` + optional
//! `setsid(2)` + optional `sandbox_init(3)`), and `execv(2)`s into the
//! staged `--exec-file`. The Linux-only flags (`--cgroup`, `--netns`,
//! `--new-pid-ns`) are accepted-and-warned per `specs/40-jailer.md` § 2.2.
//!
//! ## Crate-level lints
//!
//! * `unsafe_code` is allowed for the privilege-drop syscall boundary. The syscalls (`chroot` /
//!   `setuid` / `setgid` / `setrlimit` / `setsid` / `execv`) and `sandbox_init(3)` are all `extern
//!   "C"` calls with no safe wrapper. Total `unsafe` LOC ≈ 30, all in [`mod@env`], [`resource`],
//!   [`sandbox`] and [`sequence`], each block prefixed with `// SAFETY:` per
//!   `specs/70-security.md`.
//! * `clippy::disallowed_methods` is allowed crate-wide because squib-jail is a strictly
//!   synchronous setuid shim. The workspace clippy.toml maps `std::fs::*` to their `tokio::fs::*`
//!   async counterparts; that mapping is correct for the rest of the workspace and incorrect for a
//!   binary that must not pull a tokio runtime in before `execv(2)`. The replacement is documented
//!   in `specs/40-jailer.md` § 1 ("the privilege- drop work happens before squib itself runs").

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use clap::Parser;
use tracing::error;
use tracing_subscriber::EnvFilter;

mod cli;
mod env;
mod error;
mod resource;
mod sandbox;
mod sequence;

use cli::{Args, LogLevel};
use env::JailerEnv;

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    init_tracing(args.level);
    args.warn_unsupported();
    if let Err(msg) = args.validate_passthrough_argv_total() {
        error!("{msg}");
        return std::process::ExitCode::from(1);
    }

    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            error!("{err}");
            // Cap exit code at 255; libc exit codes are u8 by convention.
            #[allow(clippy::cast_sign_loss)]
            let code = err.exit_code().clamp(1, 255) as u8;
            std::process::ExitCode::from(code)
        }
    }
}

fn run(args: &Args) -> error::Result<()> {
    let env = JailerEnv::from_args(args)?;
    env.stage()?;
    sequence::run(&env)
}

fn init_tracing(level: LogLevel) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level.as_directive()));
    // Subscriber writes to stderr so daemonize's stdio redirect doesn't
    // silently swallow our last warnings.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
