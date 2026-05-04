//! `/machine-config` request and response shapes.
//!
//! Per [21-api-compat-matrix.md § 2
//! `/machine-config`](../../../specs/21-api-compat-matrix.md#machine-config):
//!
//! | field | rule |
//! |-------|------|
//! | `vcpu_count` | `1..=32` (D19 / `MAX_SUPPORTED_VCPUS`) |
//! | `mem_size_mib` | `>= 1`; upper bound enforced at controller against host RAM |
//! | `smt` | accept `false` only; `true` rejected with the upstream-shaped fault |
//! | `track_dirty_pages` | bool, default false |
//! | `cpu_template` | `Some("V1N1")` ⇒ aarch64 sysreg subset; x86 templates ⇒ A (warn) |
//! | `huge_pages` | `Some("2M")` ⇒ A (warn); `None` default |

use serde::{Deserialize, Serialize};

use super::common::{MAX_VCPU_COUNT, MemSizeMib};

/// Raw shape of the `/machine-config` PUT body, exactly as it arrives off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMachineConfig {
    /// Number of vCPUs to allocate.
    pub vcpu_count: u32,
    /// Guest RAM size, in MiB.
    pub mem_size_mib: u64,
    /// Symmetric multithreading. Must be `false` on aarch64.
    #[serde(default)]
    pub smt: bool,
    /// Enable dirty-page tracking (required for Diff snapshots).
    #[serde(default)]
    pub track_dirty_pages: bool,
    /// CPU template name. Aarch64-only `V1N1` is honored; x86 names accept-and-warn.
    #[serde(default)]
    pub cpu_template: Option<String>,
    /// Huge-page request. Currently `Some("2M")` ⇒ A (warn); `None` default.
    #[serde(default)]
    pub huge_pages: Option<String>,
}

/// Validated `/machine-config` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct MachineConfig {
    /// Validated vCPU count (`1..=32`).
    pub vcpu_count: u32,
    /// Validated RAM size in MiB (`>= 1`).
    pub mem_size_mib: MemSizeMib,
    /// Always `false` on aarch64 — the constructor rejects `true`.
    pub smt: bool,
    /// Whether dirty-page tracking is enabled.
    pub track_dirty_pages: bool,
    /// Validated CPU template (aarch64 names honored, x86 names recorded for warn).
    pub cpu_template: Option<String>,
    /// Huge-page request (`Some("2M")` ⇒ accept-and-warn).
    pub huge_pages: Option<String>,
}

impl TryFrom<RawMachineConfig> for MachineConfig {
    type Error = String;

    fn try_from(raw: RawMachineConfig) -> Result<Self, Self::Error> {
        if raw.vcpu_count == 0 || raw.vcpu_count > MAX_VCPU_COUNT {
            return Err(format!(
                "Invalid vcpu_count: must be in 1..={MAX_VCPU_COUNT}, got {}",
                raw.vcpu_count
            ));
        }
        if raw.smt {
            // R row in the compat matrix.
            return Err("Invalid arch field for SMT: SMT not supported on Apple Silicon".into());
        }
        let mem_size_mib = MemSizeMib::new(raw.mem_size_mib)?;
        if let Some(t) = raw.cpu_template.as_deref() {
            if t.is_empty() {
                return Err("Invalid cpu_template: must not be empty".into());
            }
            if t.len() > 64 {
                return Err("Invalid cpu_template: exceeds 64 bytes".into());
            }
        }
        if let Some(h) = raw.huge_pages.as_deref()
            && (h.is_empty() || h.len() > 16)
        {
            return Err("Invalid huge_pages: empty or exceeds 16 bytes".into());
        }
        Ok(Self {
            vcpu_count: raw.vcpu_count,
            mem_size_mib,
            smt: raw.smt,
            track_dirty_pages: raw.track_dirty_pages,
            cpu_template: raw.cpu_template,
            huge_pages: raw.huge_pages,
        })
    }
}

/// Raw shape of the `/machine-config` PATCH body — every field optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMachineConfigPatch {
    /// New vCPU count.
    #[serde(default)]
    pub vcpu_count: Option<u32>,
    /// New RAM size in MiB.
    #[serde(default)]
    pub mem_size_mib: Option<u64>,
    /// SMT toggle (must remain `false` on aarch64).
    #[serde(default)]
    pub smt: Option<bool>,
    /// Toggle dirty-page tracking.
    #[serde(default)]
    pub track_dirty_pages: Option<bool>,
    /// Replacement CPU template.
    #[serde(default)]
    pub cpu_template: Option<String>,
    /// Replacement huge-pages setting.
    #[serde(default)]
    pub huge_pages: Option<String>,
}

/// Validated `/machine-config` PATCH body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct MachineConfigPatch {
    /// New vCPU count, validated `1..=32` if present.
    pub vcpu_count: Option<u32>,
    /// New RAM size, validated `>= 1` if present.
    pub mem_size_mib: Option<MemSizeMib>,
    /// SMT toggle (must remain `false`).
    pub smt: Option<bool>,
    /// Dirty-page tracking toggle.
    pub track_dirty_pages: Option<bool>,
    /// Replacement CPU template.
    pub cpu_template: Option<String>,
    /// Replacement huge-pages setting.
    pub huge_pages: Option<String>,
}

impl TryFrom<RawMachineConfigPatch> for MachineConfigPatch {
    type Error = String;

    fn try_from(raw: RawMachineConfigPatch) -> Result<Self, Self::Error> {
        if let Some(v) = raw.vcpu_count
            && (v == 0 || v > MAX_VCPU_COUNT)
        {
            return Err(format!(
                "Invalid vcpu_count: must be in 1..={MAX_VCPU_COUNT}, got {v}"
            ));
        }
        if let Some(true) = raw.smt {
            return Err("Invalid arch field for SMT: SMT not supported on Apple Silicon".into());
        }
        let mem_size_mib = match raw.mem_size_mib {
            Some(v) => Some(MemSizeMib::new(v)?),
            None => None,
        };
        Ok(Self {
            vcpu_count: raw.vcpu_count,
            mem_size_mib,
            smt: raw.smt,
            track_dirty_pages: raw.track_dirty_pages,
            cpu_template: raw.cpu_template,
            huge_pages: raw.huge_pages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(vcpu: u32, mem: u64) -> RawMachineConfig {
        RawMachineConfig {
            vcpu_count: vcpu,
            mem_size_mib: mem,
            smt: false,
            track_dirty_pages: false,
            cpu_template: None,
            huge_pages: None,
        }
    }

    #[test]
    fn test_should_accept_minimum_machine_config() {
        let mc = MachineConfig::try_from(raw(1, 256)).unwrap();
        assert_eq!(mc.vcpu_count, 1);
        assert_eq!(mc.mem_size_mib.get(), 256);
    }

    #[test]
    fn test_should_accept_maximum_vcpu_count() {
        let mc = MachineConfig::try_from(raw(32, 256)).unwrap();
        assert_eq!(mc.vcpu_count, 32);
    }

    #[test]
    fn test_should_reject_zero_vcpu_count() {
        assert!(MachineConfig::try_from(raw(0, 256)).is_err());
    }

    #[test]
    fn test_should_reject_vcpu_count_above_32() {
        let err = MachineConfig::try_from(raw(33, 256)).unwrap_err();
        assert!(err.contains("vcpu_count"));
    }

    #[test]
    fn test_should_reject_smt_true_with_upstream_message() {
        let mut r = raw(1, 256);
        r.smt = true;
        let err = MachineConfig::try_from(r).unwrap_err();
        assert!(err.contains("SMT not supported on Apple Silicon"));
    }

    #[test]
    fn test_should_reject_zero_mem_size_mib() {
        assert!(MachineConfig::try_from(raw(1, 0)).is_err());
    }

    #[test]
    fn test_should_reject_unknown_fields() {
        let json = r#"{"vcpu_count":1,"mem_size_mib":256,"unexpected":true}"#;
        let res: Result<RawMachineConfig, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    #[test]
    fn test_should_validate_patch_partial_fields() {
        let raw = RawMachineConfigPatch {
            vcpu_count: Some(4),
            mem_size_mib: None,
            smt: None,
            track_dirty_pages: Some(true),
            cpu_template: None,
            huge_pages: None,
        };
        let p = MachineConfigPatch::try_from(raw).unwrap();
        assert_eq!(p.vcpu_count, Some(4));
        assert_eq!(p.track_dirty_pages, Some(true));
    }
}
