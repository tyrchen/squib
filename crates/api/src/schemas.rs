//! Request and response types that mirror Firecracker's `OpenAPI` shapes.
//!
//! Field names match upstream's swagger exactly: `snake_case` in nested JSON, derived from
//! `firecracker.yaml`. New schemas are added to this module as endpoints land.
//!
//! `state` on `InstanceInfo` is the **wire-shape** [`VmState`] enum from
//! [10-data-model.md § 2.2](../../../specs/10-data-model.md#22-instanceinfo--get-): three
//! variants only (`Not started`, `Running`, `Paused`). The richer internal
//! [`squib_core::LifecyclePhase`] is collapsed to this on every `GET /` via
//! `LifecyclePhase::wire_state`.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use squib_core::WireVmState;

/// Body of `GET /version`.
///
/// Upstream returns the literal Firecracker version string (e.g. `"1.16.0"`) so SDK
/// version sniffers continue to work. Squib emits the same string for compatibility — see
/// `specs/squib-api-compat-design.md` row `GET /version`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct VersionResponse {
    /// Firecracker-compatible version string; what SDKs see when they probe the server.
    pub firecracker_version: String,
}

/// Body of `GET /` (the `InstanceInfo` resource).
///
/// Mirrors upstream's `InstanceInfo` model (`id`, `state`, `vmm_version`, `app_name`).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// Microvm instance ID (the `--id` flag value).
    pub id: String,
    /// Current state of the microvm — wire-shape three-value enum.
    pub state: VmState,
    /// VMM build identifier — squib uses `"<firecracker-compat-version> (squib X.Y.Z)"`.
    pub vmm_version: String,
    /// Application name; we identify as `"Firecracker"` for SDK sniffing parity.
    pub app_name: String,
}

/// Wire-shape lifecycle state served by `GET /` and inside snapshot metadata.
///
/// Mirrors upstream `vmm/src/vmm_config/instance_info.rs::VmState` exactly: three
/// variants, with `NotStarted` serializing as the literal string `"Not started"` (note
/// the space + lowercase `s`). SDKs and `firectl` sniff these strings byte-for-byte;
/// any deviation breaks compatibility.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub enum VmState {
    /// VMM has started but no microvm is running. Wire string: `"Not started"`.
    #[default]
    NotStarted,
    /// Microvm has booted and at least one vCPU is active. Wire string: `"Running"`.
    Running,
    /// Microvm has booted but vCPUs are paused. Wire string: `"Paused"`.
    Paused,
}

impl VmState {
    /// The exact upstream wire string for this state.
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::NotStarted => "Not started",
            Self::Running => "Running",
            Self::Paused => "Paused",
        }
    }
}

impl From<WireVmState> for VmState {
    fn from(s: WireVmState) -> Self {
        match s {
            WireVmState::NotStarted => Self::NotStarted,
            WireVmState::Running => Self::Running,
            WireVmState::Paused => Self::Paused,
        }
    }
}

impl Serialize for VmState {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_wire_str())
    }
}

impl<'de> Deserialize<'de> for VmState {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = <&str>::deserialize(de)?;
        match s {
            "Not started" => Ok(Self::NotStarted),
            "Running" => Ok(Self::Running),
            "Paused" => Ok(Self::Paused),
            other => Err(de::Error::unknown_variant(
                other,
                &["Not started", "Running", "Paused"],
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_response_uses_snake_case_field() {
        let v = VersionResponse {
            firecracker_version: "1.16.0".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, r#"{"firecracker_version":"1.16.0"}"#);
    }

    #[test]
    fn instance_info_round_trips() {
        let original = InstanceInfo {
            id: "anonymous".into(),
            state: VmState::NotStarted,
            vmm_version: "1.16.0 (squib 0.1.0)".into(),
            app_name: "Firecracker".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: InstanceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn vm_state_serializes_to_upstream_strings_verbatim() {
        assert_eq!(
            serde_json::to_string(&VmState::NotStarted).unwrap(),
            r#""Not started""#
        );
        assert_eq!(
            serde_json::to_string(&VmState::Running).unwrap(),
            r#""Running""#
        );
        assert_eq!(
            serde_json::to_string(&VmState::Paused).unwrap(),
            r#""Paused""#
        );
    }

    #[test]
    fn vm_state_deserializes_only_upstream_strings() {
        let s: VmState = serde_json::from_str(r#""Not started""#).unwrap();
        assert_eq!(s, VmState::NotStarted);

        // PascalCase variant the squib draft used to emit must now be rejected — any
        // SDK seeing it would already have broken in production.
        assert!(serde_json::from_str::<VmState>(r#""NotStarted""#).is_err());
    }

    #[test]
    fn lifecycle_phase_collapses_to_vm_state_via_wire_state() {
        use squib_core::LifecyclePhase;

        let cases = [
            (LifecyclePhase::Uninitialized, VmState::NotStarted),
            (LifecyclePhase::NotStarted, VmState::NotStarted),
            (LifecyclePhase::Starting, VmState::NotStarted),
            (LifecyclePhase::Shutdown, VmState::NotStarted),
            (LifecyclePhase::Running, VmState::Running),
            (LifecyclePhase::Paused, VmState::Paused),
        ];
        for (phase, expected) in cases {
            assert_eq!(VmState::from(phase.wire_state()), expected, "{phase:?}");
        }
    }
}
