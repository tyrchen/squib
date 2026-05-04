//! `/boot-source` PUT body.
//!
//! Per [21-api-compat-matrix.md
//! `/boot-source`](../../../specs/21-api-compat-matrix.md#boot-source):
//!
//! - `kernel_image_path` — required; validated as `SafePath` (1024-byte cap, no NUL).
//! - `initrd_path` — optional; same validation.
//! - `boot_args` — optional; capped at 4096 bytes (Linux `COMMAND_LINE_SIZE` is typically 256–4096
//!   depending on architecture; we use 4096 as the practical upper bound). The FDT builder applies
//!   the D23 composition rule downstream — this layer passes the user value through verbatim.

use serde::{Deserialize, Serialize};

use super::common::SafePath;

/// Maximum length of the `boot_args` cmdline string, in bytes.
pub const BOOT_ARGS_MAX: usize = 4096;

/// Raw `/boot-source` PUT body, exactly off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBootSourceConfig {
    /// Host-filesystem path to the kernel image.
    pub kernel_image_path: String,
    /// Host-filesystem path to the initrd, if any.
    #[serde(default)]
    pub initrd_path: Option<String>,
    /// Kernel command line. The FDT builder applies the D23 append rules.
    #[serde(default)]
    pub boot_args: Option<String>,
}

/// Validated `/boot-source` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct BootSourceConfig {
    /// Validated kernel-image path.
    pub kernel_image_path: SafePath,
    /// Optional validated initrd path.
    pub initrd_path: Option<SafePath>,
    /// Validated kernel command line (`boot_args`).
    pub boot_args: Option<String>,
}

impl TryFrom<RawBootSourceConfig> for BootSourceConfig {
    type Error = String;

    fn try_from(raw: RawBootSourceConfig) -> Result<Self, Self::Error> {
        let kernel_image_path = SafePath::new(raw.kernel_image_path)
            .map_err(|e| format!("Invalid kernel_image_path: {e}"))?;
        let initrd_path = match raw.initrd_path {
            Some(p) => Some(SafePath::new(p).map_err(|e| format!("Invalid initrd_path: {e}"))?),
            None => None,
        };
        if let Some(args) = raw.boot_args.as_deref() {
            if args.len() > BOOT_ARGS_MAX {
                return Err(format!(
                    "Invalid boot_args: exceeds {BOOT_ARGS_MAX} bytes (got {} bytes)",
                    args.len()
                ));
            }
            if args.contains('\0') {
                return Err("Invalid boot_args: must not contain NUL bytes".into());
            }
        }
        Ok(Self {
            kernel_image_path,
            initrd_path,
            boot_args: raw.boot_args,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_minimal_boot_source() {
        let raw = RawBootSourceConfig {
            kernel_image_path: "/tmp/vmlinux.bin".into(),
            initrd_path: None,
            boot_args: None,
        };
        let cfg = BootSourceConfig::try_from(raw).unwrap();
        assert_eq!(
            cfg.kernel_image_path.as_path().as_os_str(),
            "/tmp/vmlinux.bin"
        );
        assert!(cfg.initrd_path.is_none());
    }

    #[test]
    fn test_should_reject_empty_kernel_path() {
        let raw = RawBootSourceConfig {
            kernel_image_path: String::new(),
            initrd_path: None,
            boot_args: None,
        };
        assert!(BootSourceConfig::try_from(raw).is_err());
    }

    #[test]
    fn test_should_reject_oversize_boot_args() {
        let raw = RawBootSourceConfig {
            kernel_image_path: "/tmp/vmlinux.bin".into(),
            initrd_path: None,
            boot_args: Some("a".repeat(BOOT_ARGS_MAX + 1)),
        };
        let err = BootSourceConfig::try_from(raw).unwrap_err();
        assert!(err.contains("boot_args"));
    }

    #[test]
    fn test_should_reject_nul_in_boot_args() {
        let raw = RawBootSourceConfig {
            kernel_image_path: "/tmp/vmlinux.bin".into(),
            initrd_path: None,
            boot_args: Some("rw\0bad".into()),
        };
        assert!(BootSourceConfig::try_from(raw).is_err());
    }

    #[test]
    fn test_should_reject_unknown_fields() {
        let json = r#"{"kernel_image_path":"/k","unknown":1}"#;
        assert!(serde_json::from_str::<RawBootSourceConfig>(json).is_err());
    }

    #[test]
    fn test_should_round_trip_through_serde() {
        let json = r#"{"kernel_image_path":"/tmp/k","initrd_path":"/tmp/i","boot_args":"console=ttyAMA0"}"#;
        let raw: RawBootSourceConfig = serde_json::from_str(json).unwrap();
        let cfg = BootSourceConfig::try_from(raw).unwrap();
        assert_eq!(cfg.boot_args.as_deref(), Some("console=ttyAMA0"));
    }
}
