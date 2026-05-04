//! `/metrics` PUT body.

use serde::{Deserialize, Serialize};

use super::common::SafePath;

/// Raw `/metrics` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMetricsConfig {
    /// Path to a regular file or named pipe.
    pub metrics_path: String,
}

/// Validated `/metrics` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct MetricsConfig {
    /// Validated metrics destination.
    pub metrics_path: SafePath,
}

impl TryFrom<RawMetricsConfig> for MetricsConfig {
    type Error = String;

    fn try_from(raw: RawMetricsConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            metrics_path: SafePath::new(raw.metrics_path)
                .map_err(|e| format!("Invalid metrics_path: {e}"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_minimal_metrics_config() {
        let cfg = MetricsConfig::try_from(RawMetricsConfig {
            metrics_path: "/tmp/squib.metrics".into(),
        })
        .unwrap();
        assert_eq!(cfg.metrics_path.as_path().as_os_str(), "/tmp/squib.metrics");
    }

    #[test]
    fn test_should_reject_empty_metrics_path() {
        assert!(
            MetricsConfig::try_from(RawMetricsConfig {
                metrics_path: String::new(),
            })
            .is_err()
        );
    }
}
