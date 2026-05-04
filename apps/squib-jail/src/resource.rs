//! `--resource-limit <key>=<value>` parsing and `setrlimit(2)` application.
//!
//! Supported keys mirror upstream:
//! - `fsize` → `RLIMIT_FSIZE` (max bytes for files created by the process)
//! - `no-file` → `RLIMIT_NOFILE` (one greater than the max fd opened)
//!
//! Any other key surfaces as [`crate::error::JailerError::UnsupportedResource`].

use std::io;

use crate::error::{JailerError, Result};

/// One parsed `--resource-limit` entry, ready to be handed to `setrlimit(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResourceLimit {
    /// Stable label for diagnostics (matches the upstream key string).
    pub(crate) key: &'static str,
    /// libc `RLIMIT_*` constant.
    pub(crate) resource: libc::c_int,
    /// `rlim_cur == rlim_max == value`.
    pub(crate) value: libc::rlim_t,
}

impl ResourceLimit {
    /// Parse one `<key>=<value>` pair.
    pub(crate) fn parse(spec: &str) -> Result<Self> {
        let (key_raw, value_raw) = spec
            .split_once('=')
            .ok_or_else(|| JailerError::InvalidResourceLimit(spec.to_string()))?;
        let key = key_raw.trim();
        let value_str = value_raw.trim();
        let value: libc::rlim_t = value_str
            .parse()
            .map_err(|_| JailerError::InvalidResourceLimit(spec.to_string()))?;

        let (resource, key_label): (libc::c_int, &'static str) = match key {
            "fsize" => (libc::RLIMIT_FSIZE, "fsize"),
            "no-file" => (libc::RLIMIT_NOFILE, "no-file"),
            other => return Err(JailerError::UnsupportedResource(other.to_string())),
        };

        Ok(Self {
            key: key_label,
            resource,
            value,
        })
    }

    /// Apply this limit via `setrlimit(2)`. The same value is used for both
    /// soft and hard limits — matching upstream's behaviour when a resource
    /// is provided without separate soft/hard knobs.
    pub(crate) fn apply(self) -> Result<()> {
        let rlim = libc::rlimit {
            rlim_cur: self.value,
            rlim_max: self.value,
        };
        // SAFETY: `setrlimit` writes through the pointer we hand it for the
        // duration of the call only; the struct is stack-resident and lives
        // through the syscall return. The resource constant comes from the
        // libc-provided enum so it is always in range.
        let rc = unsafe { libc::setrlimit(self.resource, &raw const rlim) };
        if rc == 0 {
            Ok(())
        } else {
            Err(JailerError::Setrlimit {
                key: self.key,
                source: io::Error::last_os_error(),
            })
        }
    }
}

/// Parse all `--resource-limit` entries up front. Returning `Vec<ResourceLimit>`
/// lets the caller hand them to `apply` after the chroot but before the
/// privilege drop, exactly where the upstream sequence applies them.
pub(crate) fn parse_all(specs: &[String]) -> Result<Vec<ResourceLimit>> {
    specs.iter().map(|s| ResourceLimit::parse(s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_file() {
        let lim = ResourceLimit::parse("no-file=1024").unwrap();
        assert_eq!(lim.key, "no-file");
        assert_eq!(lim.resource, libc::RLIMIT_NOFILE);
        assert_eq!(lim.value, 1024);
    }

    #[test]
    fn parses_fsize() {
        let lim = ResourceLimit::parse("fsize=1073741824").unwrap();
        assert_eq!(lim.key, "fsize");
        assert_eq!(lim.resource, libc::RLIMIT_FSIZE);
        assert_eq!(lim.value, 1_073_741_824);
    }

    #[test]
    fn rejects_unknown_resource() {
        let err = ResourceLimit::parse("rss=128").unwrap_err();
        assert!(matches!(err, JailerError::UnsupportedResource(s) if s == "rss"));
    }

    #[test]
    fn rejects_missing_separator() {
        let err = ResourceLimit::parse("fsize 1024").unwrap_err();
        assert!(matches!(err, JailerError::InvalidResourceLimit(_)));
    }

    #[test]
    fn rejects_non_numeric_value() {
        let err = ResourceLimit::parse("fsize=lots").unwrap_err();
        assert!(matches!(err, JailerError::InvalidResourceLimit(_)));
    }

    #[test]
    fn parse_all_accumulates() {
        let specs = vec!["no-file=1024".to_string(), "fsize=1024".to_string()];
        let parsed = parse_all(&specs).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].key, "no-file");
        assert_eq!(parsed[1].key, "fsize");
    }

    #[test]
    fn parse_all_short_circuits_on_first_error() {
        let specs = vec!["no-file=1024".to_string(), "junk".to_string()];
        assert!(parse_all(&specs).is_err());
    }
}
