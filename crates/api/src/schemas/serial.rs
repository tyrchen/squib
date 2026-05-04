//! `/serial` PUT body — guest-side serial console destination.

use serde::{Deserialize, Serialize};

use super::common::SafePath;

/// Raw `/serial` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSerialConfig {
    /// Path of the file or named pipe receiving serial output.
    pub log_path: String,
}

/// Validated `/serial` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct SerialConfig {
    /// Validated host path.
    pub log_path: SafePath,
}

impl TryFrom<RawSerialConfig> for SerialConfig {
    type Error = String;

    fn try_from(raw: RawSerialConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            log_path: SafePath::new(raw.log_path).map_err(|e| format!("Invalid log_path: {e}"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_minimal_serial_config() {
        let cfg = SerialConfig::try_from(RawSerialConfig {
            log_path: "/tmp/serial.out".into(),
        })
        .unwrap();
        assert_eq!(cfg.log_path.as_path().as_os_str(), "/tmp/serial.out");
    }

    #[test]
    fn test_should_reject_empty_log_path() {
        assert!(
            SerialConfig::try_from(RawSerialConfig {
                log_path: String::new(),
            })
            .is_err()
        );
    }
}
