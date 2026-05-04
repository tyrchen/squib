//! `/logger` PUT body.

use serde::{Deserialize, Serialize};

use super::common::SafePath;

/// Per Firecracker. Mirrors upstream's six-level vocabulary.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
pub enum LogLevel {
    /// Suppress all logging.
    Off,
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warning,
    /// Informational events.
    #[default]
    Info,
    /// Per-request debug events.
    Debug,
    /// Verbose tracing events.
    Trace,
}

/// Raw `/logger` PUT body off the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLoggerConfig {
    /// Path to a regular file or named pipe.
    pub log_path: String,
    /// Log level filter.
    #[serde(default)]
    pub level: LogLevel,
    /// Include the level in each line.
    #[serde(default)]
    pub show_level: bool,
    /// Include the `file:line` origin in each line.
    #[serde(default)]
    pub show_log_origin: bool,
    /// Tracing module filter.
    #[serde(default)]
    pub module: Option<String>,
}

/// Validated `/logger` PUT body.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct LoggerConfig {
    /// Validated log destination.
    pub log_path: SafePath,
    /// Log level.
    pub level: LogLevel,
    /// Include the level in each line.
    pub show_level: bool,
    /// Include `file:line` origin in each line.
    pub show_log_origin: bool,
    /// Optional module filter.
    pub module: Option<String>,
}

impl TryFrom<RawLoggerConfig> for LoggerConfig {
    type Error = String;

    fn try_from(raw: RawLoggerConfig) -> Result<Self, Self::Error> {
        let log_path = SafePath::new(raw.log_path).map_err(|e| format!("Invalid log_path: {e}"))?;
        if let Some(m) = raw.module.as_deref()
            && (m.is_empty() || m.len() > 128)
        {
            return Err("Invalid module: must be 1..=128 bytes".into());
        }
        Ok(Self {
            log_path,
            level: raw.level,
            show_level: raw.show_level,
            show_log_origin: raw.show_log_origin,
            module: raw.module,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_accept_minimal_logger_config() {
        let cfg = LoggerConfig::try_from(RawLoggerConfig {
            log_path: "/tmp/squib.log".into(),
            level: LogLevel::Info,
            show_level: true,
            show_log_origin: false,
            module: None,
        })
        .unwrap();
        assert_eq!(cfg.level, LogLevel::Info);
    }

    #[test]
    fn test_should_default_level_to_info() {
        let raw: RawLoggerConfig =
            serde_json::from_str(r#"{"log_path":"/tmp/squib.log"}"#).unwrap();
        assert_eq!(raw.level, LogLevel::Info);
    }
}
