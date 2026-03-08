//! Configuration module for tv.
//!
//! Handles loading and creating config from ~/.config/tv/config.toml (XDG spec).

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

const DEFAULT_CONFIG: &str = r#"# tv configuration

# Default visibility for new sessions: "hidden" or "visible"
default_visibility = "visible"

# Default permission for requests: "prompt", "allow", or "deny"
default_permission = "prompt"

# Default output format: "pretty" or "json"
output_format = "pretty"

# Number of scrollback lines to keep
history_size = 10000

# Delay in ms between key groups for tv exec (0 to disable)
send_delay_ms = 50

# Idle threshold in ms for {wait} — output is "settled" after this much silence
idle_threshold_ms = 1000

# Log level for daemon: "error", "warn", "info", "debug", or "trace"
log_level = "info"
"#;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultVisibility {
    Hidden,
    #[default]
    Visible,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultPermission {
    #[default]
    Prompt,
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Pretty,
    Json,
}

impl From<LogLevel> for tracing::Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Error => tracing::Level::ERROR,
            LogLevel::Warn => tracing::Level::WARN,
            LogLevel::Info => tracing::Level::INFO,
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Trace => tracing::Level::TRACE,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_visibility: DefaultVisibility,
    pub default_permission: DefaultPermission,
    pub output_format: OutputFormat,
    pub history_size: usize,
    pub send_delay_ms: u64,
    pub idle_threshold_ms: u64,
    pub log_level: LogLevel,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_visibility: DefaultVisibility::default(),
            default_permission: DefaultPermission::default(),
            output_format: OutputFormat::default(),
            history_size: 10000,
            send_delay_ms: 50,
            idle_threshold_ms: 1000,
            log_level: LogLevel::default(),
        }
    }
}

impl Config {
    /// Get config directory path (~/.config/tv/ on Unix)
    pub fn dir() -> Option<PathBuf> {
        // Use XDG config dir (~/.config/tv) on Unix systems
        // Respect $XDG_CONFIG_HOME if set, otherwise default to ~/.config
        #[cfg(unix)]
        {
            if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
                return Some(PathBuf::from(xdg).join("tv"));
            }
            dirs::home_dir().map(|p| p.join(".config").join("tv"))
        }

        #[cfg(not(unix))]
        {
            dirs::config_dir().map(|p| p.join("tv"))
        }
    }

    /// Get config file path
    pub fn path() -> Option<PathBuf> {
        Self::dir().map(|p| p.join("config.toml"))
    }

    /// Load config, creating default file if it doesn't exist.
    /// Returns error if config exists but is malformed.
    pub fn load() -> Result<Self> {
        let path =
            Self::path().ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?;

        if !path.exists() {
            Self::create_default()?;
        }

        let content = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("invalid config at {}: {}", path.display(), e))?;
        Ok(config)
    }

    /// Create config directory and default config file
    fn create_default() -> Result<()> {
        let dir =
            Self::dir().ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?;
        let path = dir.join("config.toml");

        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, DEFAULT_CONFIG)?;
        Ok(())
    }
}
