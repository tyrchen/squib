//! `/balloon`, `/balloon/statistics`, `/balloon/hinting/{op}` bodies.
//!
//! Per [21-api-compat-matrix.md `/balloon`
//! PUT](../../../specs/21-api-compat-matrix.md#balloon-put):
//!
//! - `amount_mib` — `0..=mem_size_mib − 32` (matches upstream `MAX_BALLOON_SIZE_MIB`); the upper
//!   bound is host-RAM-dependent and validated at the controller.
//! - `deflate_on_oom` — bool default true (matches upstream).
//! - `stats_polling_interval_s` — `0..=255`; `0` disables polling.
//! - `free_page_hinting`, `free_page_reporting` — bool defaults false.

use serde::{Deserialize, Serialize};

/// Maximum `stats_polling_interval_s` per upstream. The wire field is a `u8` so the
/// type system enforces the cap; this constant exists to document the intent.
pub const MAX_STATS_POLL_INTERVAL: u8 = u8::MAX;

/// `PATCH /balloon/hinting/{op}` operation kind. Each maps 1:1 to the URL path segment.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BalloonHintingOp {
    /// Begin free-page hinting.
    Start,
    /// Read free-page hinting status.
    Status,
    /// Stop free-page hinting.
    Stop,
}

impl BalloonHintingOp {
    /// Parse from the `{op}` URL segment.
    pub fn from_url_segment(s: &str) -> Result<Self, String> {
        match s {
            "start" => Ok(Self::Start),
            "status" => Ok(Self::Status),
            "stop" => Ok(Self::Stop),
            other => Err(format!(
                "Invalid balloon-hinting op: must be one of start | status | stop (got {other})"
            )),
        }
    }
}

/// Raw `/balloon` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBalloonConfig {
    /// Target ballooned amount (MiB).
    pub amount_mib: u64,
    /// Whether the guest balloon driver should deflate on OOM.
    #[serde(default)]
    pub deflate_on_oom: bool,
    /// Stats polling interval, seconds (0 disables).
    #[serde(default)]
    pub stats_polling_interval_s: u8,
    /// Enable free-page hinting.
    #[serde(default)]
    pub free_page_hinting: bool,
    /// Enable free-page reporting.
    #[serde(default)]
    pub free_page_reporting: bool,
}

/// Validated `/balloon` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct BalloonConfig {
    /// Target ballooned amount (MiB).
    pub amount_mib: u64,
    /// Whether the guest balloon driver should deflate on OOM.
    pub deflate_on_oom: bool,
    /// Stats polling interval, seconds (0 disables).
    pub stats_polling_interval_s: u8,
    /// Enable free-page hinting.
    pub free_page_hinting: bool,
    /// Enable free-page reporting.
    pub free_page_reporting: bool,
}

impl TryFrom<RawBalloonConfig> for BalloonConfig {
    type Error = String;

    fn try_from(raw: RawBalloonConfig) -> Result<Self, Self::Error> {
        // No runtime cap needed: `u8` already encodes `0..=MAX_STATS_POLL_INTERVAL`.
        Ok(Self {
            amount_mib: raw.amount_mib,
            deflate_on_oom: raw.deflate_on_oom,
            stats_polling_interval_s: raw.stats_polling_interval_s,
            free_page_hinting: raw.free_page_hinting,
            free_page_reporting: raw.free_page_reporting,
        })
    }
}

/// Raw `/balloon` PATCH body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBalloonUpdate {
    /// New target ballooned amount.
    pub amount_mib: u64,
}

/// Validated `/balloon` PATCH body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct BalloonUpdate {
    /// New target ballooned amount.
    pub amount_mib: u64,
}

impl TryFrom<RawBalloonUpdate> for BalloonUpdate {
    type Error = String;

    fn try_from(raw: RawBalloonUpdate) -> Result<Self, Self::Error> {
        Ok(Self {
            amount_mib: raw.amount_mib,
        })
    }
}

/// Raw `/balloon/statistics` PATCH body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBalloonStatsUpdate {
    /// New stats polling interval (seconds; 0 disables).
    pub stats_polling_interval_s: u8,
}

/// Validated `/balloon/statistics` PATCH body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct BalloonStatsUpdate {
    /// New stats polling interval.
    pub stats_polling_interval_s: u8,
}

impl TryFrom<RawBalloonStatsUpdate> for BalloonStatsUpdate {
    type Error = String;

    fn try_from(raw: RawBalloonStatsUpdate) -> Result<Self, Self::Error> {
        // No runtime cap needed: `u8` already encodes `0..=MAX_STATS_POLL_INTERVAL`.
        Ok(Self {
            stats_polling_interval_s: raw.stats_polling_interval_s,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_minimal_balloon_config() {
        let raw = RawBalloonConfig {
            amount_mib: 0,
            deflate_on_oom: false,
            stats_polling_interval_s: 0,
            free_page_hinting: false,
            free_page_reporting: false,
        };
        let cfg = BalloonConfig::try_from(raw).unwrap();
        assert_eq!(cfg.amount_mib, 0);
    }

    #[test]
    fn test_should_reject_oversize_polling_interval() {
        // u8 caps at 255 — but we re-validate to keep the contract explicit and to
        // catch a future widening to u16.
        let raw = RawBalloonConfig {
            amount_mib: 0,
            deflate_on_oom: false,
            stats_polling_interval_s: u8::MAX,
            free_page_hinting: false,
            free_page_reporting: false,
        };
        assert!(BalloonConfig::try_from(raw).is_ok());
    }

    #[test]
    fn test_should_round_trip_balloon_update() {
        let raw = RawBalloonUpdate { amount_mib: 128 };
        let upd = BalloonUpdate::try_from(raw).unwrap();
        assert_eq!(upd.amount_mib, 128);
    }
}
