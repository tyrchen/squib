//! `PATCH /vm` body — pause / resume.

use serde::{Deserialize, Serialize};

/// State change requested through `PATCH /vm`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum VmStateChange {
    /// Pause vCPUs.
    Paused,
    /// Resume vCPUs.
    Resumed,
}

/// Raw `PATCH /vm` body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVmPatch {
    /// Target state.
    pub state: VmStateChange,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_deserialize_paused() {
        let raw: RawVmPatch = serde_json::from_str(r#"{"state":"Paused"}"#).unwrap();
        assert_eq!(raw.state, VmStateChange::Paused);
    }

    #[test]
    fn test_should_deserialize_resumed() {
        let raw: RawVmPatch = serde_json::from_str(r#"{"state":"Resumed"}"#).unwrap();
        assert_eq!(raw.state, VmStateChange::Resumed);
    }

    #[test]
    fn test_should_reject_unknown_state() {
        assert!(serde_json::from_str::<RawVmPatch>(r#"{"state":"Halted"}"#).is_err());
    }
}
