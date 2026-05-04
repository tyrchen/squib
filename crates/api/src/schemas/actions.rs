//! `/actions` PUT body.
//!
//! Per [21-api-compat-matrix.md `/actions`
//! PUT](../../../specs/21-api-compat-matrix.md#actions-put):
//!
//! - `InstanceStart` — `F`.
//! - `FlushMetrics` — `F`.
//! - `SendCtrlAltDel` — `R` (x86-only; rejected with `fault_message`).

use serde::{Deserialize, Serialize};

/// Action verb upstream Firecracker supports on `/actions`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum InstanceAction {
    /// Start the microVM (boot orchestration).
    InstanceStart,
    /// Flush queued metrics to the metrics destination.
    FlushMetrics,
    /// `Ctrl+Alt+Del` — x86-only; rejected on aarch64.
    SendCtrlAltDel,
}

/// Raw `/actions` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawInstanceActionInfo {
    /// Action to dispatch.
    pub action_type: InstanceAction,
}

/// Validated `/actions` PUT body. The `SendCtrlAltDel` rejection happens at the
/// controller (so the wire shape stays compatible with x86-only Firecracker SDKs that
/// might post the verb without checking architecture).
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct InstanceActionInfo {
    /// Action to dispatch.
    pub action_type: InstanceAction,
}

impl From<RawInstanceActionInfo> for InstanceActionInfo {
    fn from(raw: RawInstanceActionInfo) -> Self {
        Self {
            action_type: raw.action_type,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_deserialize_instance_start_pascal_case() {
        let json = r#"{"action_type":"InstanceStart"}"#;
        let raw: RawInstanceActionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(raw.action_type, InstanceAction::InstanceStart);
    }

    #[test]
    fn test_should_deserialize_send_ctrl_alt_del_for_arch_rejection_downstream() {
        let json = r#"{"action_type":"SendCtrlAltDel"}"#;
        let raw: RawInstanceActionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(raw.action_type, InstanceAction::SendCtrlAltDel);
    }

    #[test]
    fn test_should_reject_unknown_action() {
        let json = r#"{"action_type":"Frobnicate"}"#;
        assert!(serde_json::from_str::<RawInstanceActionInfo>(json).is_err());
    }
}
