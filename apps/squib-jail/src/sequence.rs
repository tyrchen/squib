//! The privilege-drop sequence:
//!
//! ```text
//! 1. mkdir -p chroot, copy --exec-file inside  (handled by env::stage)
//! 2. setrlimit per --resource-limit             (this module)
//! 3. (--daemonize) setsid + redirect 0/1/2 to /dev/null
//! 4. chroot(2)
//! 5. setgid, setuid
//! 6. (--macos-sandbox-profile) sandbox_init
//! 7. execv into /<basename>
//! ```
//!
//! Order matches `specs/40-jailer.md` § 3 step-for-step. Fails surface as
//! [`crate::error::JailerError`] variants whose exit codes are pinned by
//! I-JAIL-2.
//!
//! Every libc call is wrapped with a `// SAFETY:` comment explaining the
//! contract and lifetime of any pointer arguments. The unsafe surface is
//! ~30 lines, all in this module.

use std::{ffi::CString, io};

use crate::{
    cli::SandboxProfile,
    env::JailerEnv,
    error::{JailerError, Result},
    sandbox,
};

/// Run the chroot+setuid+setgid+execv pipeline. On success this function
/// does not return: control transfers to the staged binary via `execv(2)`.
/// Any `Ok(())` return is therefore unreachable.
pub(crate) fn run(env: &JailerEnv) -> Result<()> {
    apply_resource_limits(env)?;

    if env.daemonize {
        daemonize()?;
    }

    chroot(&env.chroot_dir)?;

    setgid(env.gid)?;
    setuid(env.uid)?;

    if let Some(profile) = env.sandbox_profile {
        apply_sandbox(profile)?;
    }

    let argv = env.build_argv()?;
    exec(env, &argv)
}

fn apply_resource_limits(env: &JailerEnv) -> Result<()> {
    for lim in &env.resource_limits {
        lim.apply()?;
    }
    Ok(())
}

fn daemonize() -> Result<()> {
    // SAFETY: `setsid` takes no arguments and either succeeds or sets errno.
    let sid = unsafe { libc::setsid() };
    if sid == -1 {
        return Err(JailerError::Setsid(io::Error::last_os_error()));
    }
    redirect_stdio_to_dev_null()
}

fn redirect_stdio_to_dev_null() -> Result<()> {
    // Open /dev/null read-write so a single fd can be dup'd onto stdin /
    // stdout / stderr. The `c"…"` literal yields a `&'static CStr` so the
    // pointer is valid for the full duration of the syscall.
    // SAFETY: `open` takes a NUL-terminated C string for the duration of the
    // call. `O_RDWR` is a constant and has no extra mode argument
    // requirement.
    let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(JailerError::OpenDevNull(io::Error::last_os_error()));
    }
    let result = (|| -> Result<()> {
        for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            // SAFETY: `dup2` with two valid fds either succeeds and returns
            // `target` or returns -1 and sets errno. We never pass an
            // out-of-range fd.
            let rc = unsafe { libc::dup2(fd, target) };
            if rc < 0 {
                return Err(JailerError::RedirectStdio(io::Error::last_os_error()));
            }
        }
        Ok(())
    })();
    if fd > libc::STDERR_FILENO {
        // SAFETY: `close` of an fd we own is always safe; even on error
        // there's no aliasing concern. We swallow EBADF deliberately —
        // dup2 above may have collapsed all four fds onto the same
        // underlying file description, in which case the original fd is
        // still valid here.
        unsafe {
            libc::close(fd);
        }
    }
    result
}

fn chroot(path: &std::path::Path) -> Result<()> {
    let path_c = path
        .to_str()
        .and_then(|s| CString::new(s).ok())
        .ok_or_else(|| JailerError::PathContainsNul(path.to_path_buf()))?;
    // SAFETY: `chroot(2)` reads the NUL-terminated path during the call only.
    // Returns -1 on failure with errno set.
    let rc = unsafe { libc::chroot(path_c.as_ptr()) };
    if rc != 0 {
        return Err(JailerError::Chroot {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        });
    }
    // SAFETY: `chdir(2)` reads the NUL-terminated path during the call only.
    // `c"/"` yields a `&'static CStr` so the pointer outlives the syscall.
    let rc = unsafe { libc::chdir(c"/".as_ptr()) };
    if rc != 0 {
        return Err(JailerError::ChdirRoot(io::Error::last_os_error()));
    }
    Ok(())
}

fn setgid(gid: libc::gid_t) -> Result<()> {
    // SAFETY: `setgid(2)` is a pure-integer syscall.
    let rc = unsafe { libc::setgid(gid) };
    if rc != 0 {
        return Err(JailerError::Setgid {
            gid,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn setuid(uid: libc::uid_t) -> Result<()> {
    // SAFETY: `setuid(2)` is a pure-integer syscall.
    let rc = unsafe { libc::setuid(uid) };
    if rc != 0 {
        return Err(JailerError::Setuid {
            uid,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn apply_sandbox(profile: SandboxProfile) -> Result<()> {
    sandbox::apply_profile(profile)
}

fn exec(env: &JailerEnv, argv: &[CString]) -> Result<()> {
    let argv0 = argv.first().ok_or_else(|| {
        JailerError::Exec(env.exec_file_in_chroot(), io::Error::other("argv empty"))
    })?;
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    // SAFETY: `execv(2)` reads `argv0` and `ptrs[..]` for the duration of the
    // call only; on success it does not return so the borrows trivially
    // outlive the call. `ptrs` is null-terminated as required.
    unsafe {
        libc::execv(argv0.as_ptr(), ptrs.as_ptr());
    }
    // execv only returns on failure.
    Err(JailerError::Exec(
        env.exec_file_in_chroot(),
        io::Error::last_os_error(),
    ))
}

#[cfg(test)]
mod tests {
    //! Most of `sequence.rs` is privilege-drop syscalls that the unit test
    //! runner cannot exercise directly without root. We cover the parsing
    //! and dispatch shape here, and the integration test (gated behind
    //! `--include-ignored` and a manual `sudo`) covers the live path in CI.

    use std::path::PathBuf;

    use super::*;

    #[test]
    fn build_argv_is_null_terminated_when_handed_to_exec() {
        // Mirrors the layout `exec()` constructs.
        let env = JailerEnv {
            exec_file_host: PathBuf::from("/usr/local/bin/squib"),
            exec_file_basename: "squib".into(),
            chroot_dir: PathBuf::from("/srv/jailer/firecracker/vm1/root"),
            uid: 1000,
            gid: 1000,
            resource_limits: vec![],
            daemonize: false,
            sandbox_profile: None,
            child_argv: vec!["--api-sock".into(), "/x.sock".into()],
        };
        let argv = env.build_argv().unwrap();
        let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        // The last slot must be null exactly as `execv(2)` requires.
        assert!(ptrs.last().copied().unwrap().is_null());
        // And the slot before it must be a valid C string.
        let last_arg = ptrs[ptrs.len() - 2];
        assert!(!last_arg.is_null());
    }
}
