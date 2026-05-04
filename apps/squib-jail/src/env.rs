//! Environment validation: turn raw clap args into a [`JailerEnv`] that is
//! ready to drive the privilege-drop sequence in [`crate::sequence`].
//!
//! Validation discipline matches `specs/70-security.md` § 4: regex/charset
//! allowlists, length caps, NUL-byte rejection, canonicalize-after-open for
//! every path that crosses a libc boundary.

use std::{
    ffi::CString,
    fs, io,
    os::{
        fd::{AsRawFd as _, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::PermissionsExt},
    },
    path::{Path, PathBuf},
};

use crate::{
    cli::{Args, SandboxProfile},
    error::{JailerError, Result},
    resource::{self, ResourceLimit},
};

/// Maximum byte length of `--id`, matching upstream `jailer`'s `MAX_ID_LENGTH = 64`.
const MAX_ID_BYTES: usize = 64;

/// Validated `JailerEnv` ready to drive the chroot+setuid+execv sequence.
///
/// The instance id passed via `--id` is consumed during construction (it's
/// embedded in [`Self::chroot_dir`] via [`compose_chroot_dir`]); we don't
/// keep it as a separate field — the chroot path is the canonical source
/// of truth post-validation, and downstream code that needs the id
/// extracts it from there.
#[derive(Debug)]
pub(crate) struct JailerEnv {
    /// Canonicalized absolute path to the binary we'll exec into.
    pub(crate) exec_file_host: PathBuf,
    /// File name of `exec_file_host`, used as the in-chroot path.
    pub(crate) exec_file_basename: String,
    /// `<chroot_base_dir>/firecracker/<id>/root` per `specs/40-jailer.md` § 3.
    pub(crate) chroot_dir: PathBuf,
    /// Numeric uid the jailer setuids to.
    pub(crate) uid: libc::uid_t,
    /// Numeric gid the jailer setgids to.
    pub(crate) gid: libc::gid_t,
    /// Pre-parsed resource limits to apply via `setrlimit(2)`.
    pub(crate) resource_limits: Vec<ResourceLimit>,
    /// `--daemonize` requested.
    pub(crate) daemonize: bool,
    /// Optional `sandbox_init(3)` profile to apply just before exec.
    pub(crate) sandbox_profile: Option<SandboxProfile>,
    /// argv to pass through to the staged binary.
    pub(crate) child_argv: Vec<String>,
}

impl JailerEnv {
    /// Validate raw clap args and resolve every path / id / numeric value.
    /// Run once per process before any privilege manipulation.
    pub(crate) fn from_args(args: &Args) -> Result<Self> {
        let id = validate_id(&args.id)?;
        let uid: libc::uid_t = args.uid;
        let gid: libc::gid_t = args.gid;
        let resource_limits = resource::parse_all(&args.resource_limits)?;

        let exec_file_host = canonicalize_exec_file(&args.exec_file)?;
        let exec_file_basename = exec_file_host
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| JailerError::ExecFileNotRegular(exec_file_host.clone()))?
            .to_string();

        let chroot_dir = compose_chroot_dir(&args.chroot_base_dir, id);

        Ok(Self {
            exec_file_host,
            exec_file_basename,
            chroot_dir,
            uid,
            gid,
            resource_limits,
            daemonize: args.daemonize,
            sandbox_profile: args.macos_sandbox_profile,
            child_argv: args.passthrough_argv.clone(),
        })
    }

    /// Inside-chroot absolute path the binary will live at after staging
    /// (`/<basename>`).
    pub(crate) fn exec_file_in_chroot(&self) -> PathBuf {
        PathBuf::from("/").join(&self.exec_file_basename)
    }

    /// Build the `argv` slice the jailer hands to `execv(2)`.
    /// Convention: `argv[0]` is the in-chroot path of the staged binary,
    /// followed by every value from `--` onwards.
    pub(crate) fn build_argv(&self) -> Result<Vec<CString>> {
        let argv0 = path_to_cstring(&self.exec_file_in_chroot())?;
        let mut out = Vec::with_capacity(self.child_argv.len() + 1);
        out.push(argv0);
        for raw in &self.child_argv {
            out.push(arg_to_cstring(raw)?);
        }
        Ok(out)
    }

    /// Stage `--exec-file` inside the chroot directory. Creates the chroot
    /// path tree first; both steps fail with [`JailerError::CreateChroot`]
    /// or [`JailerError::StageExecFile`] respectively.
    ///
    /// Once the chroot tree is created, the directory mode is forced to `0o755`
    /// — `create_dir_all` inherits the caller's umask (typically `022`, but
    /// service managers sometimes start launchers with `002`), and a chroot tree
    /// readable by group / world is a real lateral-movement risk on a multi-tenant
    /// host (see `specs/70-security.md` § 5).
    ///
    /// The `--exec-file` copy goes through an `O_NOFOLLOW + fstat` open of the
    /// canonical source path (closing the TOCTOU window between
    /// [`canonicalize_exec_file`] and the copy: a racing rename of the canonical
    /// path can no longer slip a different binary in under us).
    pub(crate) fn stage(&self) -> Result<()> {
        fs::create_dir_all(&self.chroot_dir).map_err(|e| JailerError::CreateChroot {
            path: self.chroot_dir.clone(),
            source: e,
        })?;
        // I-JAIL-1 expects the chroot tree to be operator-readable but never
        // group/world writable. Setting `0o755` explicitly defeats whatever
        // umask the caller arrived with.
        fs::set_permissions(&self.chroot_dir, fs::Permissions::from_mode(0o755)).map_err(|e| {
            JailerError::CreateChroot {
                path: self.chroot_dir.clone(),
                source: e,
            }
        })?;
        let dst = self.chroot_dir.join(&self.exec_file_basename);
        copy_via_nofollow(&self.exec_file_host, &dst).map_err(|e| JailerError::StageExecFile {
            src: self.exec_file_host.clone(),
            dst: dst.clone(),
            source: e,
        })?;
        Ok(())
    }
}

/// Copy `src` → `dst` while closing the TOCTOU window on `src`. The pattern is:
///
/// 1. `open(src, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)` — refuses to traverse a symlink that
///    materialised between [`canonicalize_exec_file`] and now.
/// 2. `fstat(src_fd)` — verify the open fd is a regular file. Anyone who rename(2)'d the canonical
///    path between canonicalize and open might have swapped in a directory, FIFO, or device node.
/// 3. `std::io::copy` from the file backed by the fd into the destination.
fn copy_via_nofollow(src: &Path, dst: &Path) -> io::Result<u64> {
    let src_c = path_to_cstring(src)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "src path contains NUL"))?;
    // SAFETY: `open` reads `src_c` as a NUL-terminated C string for the
    // duration of the call. `O_NOFOLLOW | O_CLOEXEC` are constants and have
    // no extra mode-argument requirement when O_CREAT is absent.
    let raw = unsafe {
        libc::open(
            src_c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a freshly-opened fd we own; wrapping it in `OwnedFd`
    // transfers ownership to RAII and ensures `close(2)` runs even on the
    // error paths below.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fstat` reads the metadata of the fd into the `stat` buffer.
    // We pass a pointer to a stack-allocated `MaybeUninit<libc::stat>` whose
    // size matches what fstat writes; on success the bytes are initialised.
    let rc = unsafe { libc::fstat(owned.as_raw_fd(), st.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: rc == 0 means fstat fully populated the stat struct.
    let st = unsafe { st.assume_init() };
    let mode_type = st.st_mode & libc::S_IFMT;
    if mode_type != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "exec-file is not a regular file (post-canonicalise check failed)",
        ));
    }
    let mut src_file = fs::File::from(owned);
    let mut dst_file = fs::File::create(dst)?;
    let bytes = io::copy(&mut src_file, &mut dst_file)?;
    Ok(bytes)
}

/// `--id` charset / length validator. Matches upstream jailer's
/// `^[A-Za-z0-9_-]{1,64}$` exactly so launchers that generate hyphenated
/// ids — `firecracker-go-sdk`, `firecracker-containerd`, `weaveworks/ignite`
/// all do — succeed without a renaming pass. The hyphen is genuinely safe
/// in the resulting chroot path (`<base>/firecracker/<id>/root`); the rest
/// of the id bytes are charset-checked too, so this is not a path-traversal
/// vector.
pub(crate) fn validate_id(raw: &str) -> Result<&str> {
    if raw.is_empty() {
        return Err(JailerError::InvalidInstanceId("empty".to_string()));
    }
    if raw.len() > MAX_ID_BYTES {
        return Err(JailerError::InvalidInstanceId(format!(
            "id is {} bytes; max {MAX_ID_BYTES}",
            raw.len()
        )));
    }
    let ok = raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !ok {
        return Err(JailerError::InvalidInstanceId(raw.to_string()));
    }
    Ok(raw)
}

fn canonicalize_exec_file(path: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|e| JailerError::Canonicalize {
        path: path.to_path_buf(),
        source: e,
    })?;
    // First-pass check on the canonicalised path. The authoritative regular-file
    // check happens later inside [`copy_via_nofollow`] against an `O_NOFOLLOW`-
    // opened fd — this `metadata()` call only earlies-out the obvious "the
    // operator pointed at a directory" case at validate time so the operator
    // gets a clear error before any chroot creation work runs.
    let meta = fs::metadata(&canonical).map_err(|e| JailerError::Canonicalize {
        path: canonical.clone(),
        source: e,
    })?;
    if !meta.is_file() {
        return Err(JailerError::ExecFileNotRegular(canonical));
    }
    Ok(canonical)
}

/// `<base>/firecracker/<id>/root` per `specs/40-jailer.md` § 3 — kept under a
/// `firecracker/` subdirectory so squib-jail and an upstream-jailer install
/// can coexist on the same operator's machine without colliding chroots.
pub(crate) fn compose_chroot_dir(base: &Path, id: &str) -> PathBuf {
    base.join("firecracker").join(id).join("root")
}

fn path_to_cstring(p: &Path) -> Result<CString> {
    // Paths on Darwin are bag-of-bytes (HFS+/APFS preserves any byte sequence
    // except NUL and `/`). Going through `OsStr::as_bytes` instead of `to_str`
    // means we accept non-UTF-8 paths the operator may genuinely have on disk
    // — only the interior-NUL case is the real failure here.
    let bytes = p.as_os_str().as_bytes();
    CString::new(bytes).map_err(|_| JailerError::PathContainsNul(p.to_path_buf()))
}

fn arg_to_cstring(raw: &str) -> Result<CString> {
    CString::new(raw).map_err(|_| JailerError::PathContainsNul(PathBuf::from(raw)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_id_accepts_upstream_charset() {
        assert!(validate_id("vm_0").is_ok());
        assert!(validate_id("V").is_ok());
        assert!(validate_id("0123456789").is_ok());
        // Hyphens are part of the upstream `^[A-Za-z0-9_-]{1,64}$`
        // allowlist; firecracker-go-sdk and ignite both generate
        // hyphenated ids.
        assert!(validate_id("firecracker-vm-1").is_ok());
        assert!(validate_id("a-b-c").is_ok());
    }

    #[test]
    fn validate_id_rejects_dots_and_traversal_attempts() {
        assert!(matches!(
            validate_id("vm.0"),
            Err(JailerError::InvalidInstanceId(_))
        ));
        assert!(matches!(
            validate_id("../etc"),
            Err(JailerError::InvalidInstanceId(_))
        ));
        assert!(matches!(
            validate_id("vm/foo"),
            Err(JailerError::InvalidInstanceId(_))
        ));
    }

    #[test]
    fn validate_id_rejects_empty_and_overlong() {
        assert!(matches!(
            validate_id(""),
            Err(JailerError::InvalidInstanceId(_))
        ));
        let huge = "a".repeat(MAX_ID_BYTES + 1);
        assert!(matches!(
            validate_id(&huge),
            Err(JailerError::InvalidInstanceId(_))
        ));
    }

    #[test]
    fn compose_chroot_dir_matches_upstream_shape() {
        let p = compose_chroot_dir(Path::new("/srv/jailer"), "vm1");
        assert_eq!(p, Path::new("/srv/jailer/firecracker/vm1/root"));
    }

    #[test]
    fn canonicalize_exec_file_rejects_directory() {
        // The crate root is a directory; canonicalize succeeds, then the
        // is_file() check fails — that's the path we want to verify.
        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let err = canonicalize_exec_file(crate_root).unwrap_err();
        assert!(matches!(err, JailerError::ExecFileNotRegular(_)));
    }

    #[test]
    fn canonicalize_exec_file_rejects_missing() {
        let bogus = Path::new("/this/path/does/not/exist/squib");
        let err = canonicalize_exec_file(bogus).unwrap_err();
        assert!(matches!(err, JailerError::Canonicalize { .. }));
    }

    #[test]
    fn build_argv_prepends_in_chroot_path() {
        let env = JailerEnv {
            exec_file_host: PathBuf::from("/usr/local/bin/squib"),
            exec_file_basename: "squib".into(),
            chroot_dir: PathBuf::from("/srv/jailer/firecracker/vm1/root"),
            uid: 1000,
            gid: 1000,
            resource_limits: vec![],
            daemonize: false,
            sandbox_profile: None,
            child_argv: vec!["--api-sock".into(), "/run/firecracker.socket".into()],
        };
        let argv = env.build_argv().unwrap();
        assert_eq!(argv[0].to_bytes(), b"/squib");
        assert_eq!(argv[1].to_bytes(), b"--api-sock");
        assert_eq!(argv[2].to_bytes(), b"/run/firecracker.socket");
    }

    #[test]
    fn build_argv_rejects_nul_byte_in_child_arg() {
        let env = JailerEnv {
            exec_file_host: PathBuf::from("/usr/local/bin/squib"),
            exec_file_basename: "squib".into(),
            chroot_dir: PathBuf::from("/srv/jailer/firecracker/vm1/root"),
            uid: 1000,
            gid: 1000,
            resource_limits: vec![],
            daemonize: false,
            sandbox_profile: None,
            child_argv: vec!["evil\0arg".into()],
        };
        assert!(matches!(
            env.build_argv().unwrap_err(),
            JailerError::PathContainsNul(_)
        ));
    }
}
