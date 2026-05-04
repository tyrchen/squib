//! `/cpu-config` PUT body.
//!
//! Per [21-api-compat-matrix.md `/cpu-config`
//! PUT](../../../specs/21-api-compat-matrix.md#cpu-config-put):
//!
//! - aarch64 `reg_modifiers` and `vcpu_features` — `F (best-effort)`.
//! - x86 `cpuid_modifiers` and `msr_modifiers` — `A` (accept-and-warn).
//! - `kvm_capabilities` — `A`.
//!
//! At this layer we keep the shape loose (each subtree is a `serde_json::Value`) and
//! defer the per-register honor/warn decision to the VMM. Validation here is structural:
//! deny unknown top-level fields and enforce the per-class collection caps.

use serde::{Deserialize, Serialize};

/// Maximum number of `reg_modifiers` rows we accept; arbitrary but bounded.
pub const MAX_REG_MODIFIERS: usize = 256;

/// Raw `/cpu-config` PUT body off the wire.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCpuConfig {
    /// aarch64 `reg_modifiers`.
    #[serde(default)]
    pub reg_modifiers: Option<Vec<serde_json::Value>>,
    /// aarch64 `vcpu_features`.
    #[serde(default)]
    pub vcpu_features: Option<Vec<serde_json::Value>>,
    /// x86 `cpuid_modifiers` (accept-and-warn).
    #[serde(default)]
    pub cpuid_modifiers: Option<Vec<serde_json::Value>>,
    /// x86 `msr_modifiers` (accept-and-warn).
    #[serde(default)]
    pub msr_modifiers: Option<Vec<serde_json::Value>>,
    /// `kvm_capabilities` (accept-and-warn).
    #[serde(default)]
    pub kvm_capabilities: Option<Vec<serde_json::Value>>,
}

/// Validated `/cpu-config` PUT body.
#[derive(Debug, Clone, Default, Serialize)]
#[non_exhaustive]
pub struct CpuConfig {
    /// aarch64 `reg_modifiers`, capped at `MAX_REG_MODIFIERS`.
    pub reg_modifiers: Vec<serde_json::Value>,
    /// aarch64 `vcpu_features`, capped at `MAX_REG_MODIFIERS`.
    pub vcpu_features: Vec<serde_json::Value>,
    /// x86 `cpuid_modifiers` — recorded, will warn at the controller.
    pub cpuid_modifiers: Vec<serde_json::Value>,
    /// x86 `msr_modifiers` — recorded, will warn at the controller.
    pub msr_modifiers: Vec<serde_json::Value>,
    /// `kvm_capabilities` — recorded, will warn at the controller.
    pub kvm_capabilities: Vec<serde_json::Value>,
}

fn cap_check<T>(field: &str, v: &[T]) -> Result<(), String> {
    if v.len() > MAX_REG_MODIFIERS {
        return Err(format!(
            "Invalid cpu-config: {field} exceeds {MAX_REG_MODIFIERS} entries"
        ));
    }
    Ok(())
}

impl TryFrom<RawCpuConfig> for CpuConfig {
    type Error = String;

    fn try_from(raw: RawCpuConfig) -> Result<Self, Self::Error> {
        let reg_modifiers = raw.reg_modifiers.unwrap_or_default();
        let vcpu_features = raw.vcpu_features.unwrap_or_default();
        let cpuid_modifiers = raw.cpuid_modifiers.unwrap_or_default();
        let msr_modifiers = raw.msr_modifiers.unwrap_or_default();
        let kvm_capabilities = raw.kvm_capabilities.unwrap_or_default();
        cap_check("reg_modifiers", &reg_modifiers)?;
        cap_check("vcpu_features", &vcpu_features)?;
        cap_check("cpuid_modifiers", &cpuid_modifiers)?;
        cap_check("msr_modifiers", &msr_modifiers)?;
        cap_check("kvm_capabilities", &kvm_capabilities)?;
        Ok(Self {
            reg_modifiers,
            vcpu_features,
            cpuid_modifiers,
            msr_modifiers,
            kvm_capabilities,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_empty_cpu_config() {
        let cfg = CpuConfig::try_from(RawCpuConfig::default()).unwrap();
        assert!(cfg.reg_modifiers.is_empty());
    }

    #[test]
    fn test_should_reject_oversize_reg_modifiers() {
        let raw = RawCpuConfig {
            reg_modifiers: Some(vec![serde_json::Value::Null; MAX_REG_MODIFIERS + 1]),
            ..RawCpuConfig::default()
        };
        assert!(CpuConfig::try_from(raw).is_err());
    }
}
