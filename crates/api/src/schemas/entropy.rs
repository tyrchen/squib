//! `/entropy` PUT body — virtio-rng configuration.

use serde::{Deserialize, Serialize};

/// Raw `/entropy` PUT body off the wire.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEntropyConfig {
    /// Optional rate limiter passthrough.
    #[serde(default)]
    pub rate_limiter: Option<serde_json::Value>,
}

/// Validated `/entropy` PUT body. The device has only one tunable today: a rate
/// limiter, which is passthrough-validated by the device layer.
#[derive(Debug, Clone, Default, Serialize)]
#[non_exhaustive]
pub struct EntropyConfig {
    /// Optional rate limiter passthrough.
    pub rate_limiter: Option<serde_json::Value>,
}

impl TryFrom<RawEntropyConfig> for EntropyConfig {
    type Error = String;

    fn try_from(raw: RawEntropyConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            rate_limiter: raw.rate_limiter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_empty_entropy_config() {
        let cfg = EntropyConfig::try_from(RawEntropyConfig::default()).unwrap();
        assert!(cfg.rate_limiter.is_none());
    }

    #[test]
    fn test_should_reject_unknown_fields() {
        let json = r#"{"unexpected":1}"#;
        assert!(serde_json::from_str::<RawEntropyConfig>(json).is_err());
    }
}
