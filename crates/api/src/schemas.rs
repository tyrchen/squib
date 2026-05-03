//! Request and response types that mirror Firecracker's `OpenAPI` shapes.
//!
//! Field names match upstream's swagger exactly: `snake_case` in nested JSON, derived from
//! `firecracker.yaml`. New schemas are added to this module as endpoints land.

use serde::{Deserialize, Serialize};

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
    /// Current state of the microvm.
    pub state: InstanceState,
    /// VMM build identifier — squib uses `"<firecracker-compat-version> (squib X.Y.Z)"`.
    pub vmm_version: String,
    /// Application name; we identify as `"Firecracker"` for SDK sniffing parity.
    pub app_name: String,
}

/// Lifecycle state surfaced via `GET /` and inside snapshot metadata.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum InstanceState {
    /// VMM has started but no microvm is running.
    NotStarted,
    /// Microvm has booted and vCPUs are running.
    Running,
    /// Microvm has booted but vCPUs are paused.
    Paused,
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
            state: InstanceState::NotStarted,
            vmm_version: "1.16.0 (squib 0.1.0)".into(),
            app_name: "Firecracker".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: InstanceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn instance_state_renders_as_pascal_case() {
        let json = serde_json::to_string(&InstanceState::NotStarted).unwrap();
        assert_eq!(json, r#""NotStarted""#);
    }
}
