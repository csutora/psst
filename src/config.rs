use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// notification level for event type filters
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyLevel {
    /// suppress entirely on this device
    Off,
    /// notification without sound
    Silent,
    /// notification with sound
    Noisy,
}

impl Default for NotifyLevel {
    fn default() -> Self {
        Self::Off
    }
}

/// per-room notification override
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomNotifyLevel {
    /// notify for all messages
    All,
    /// only notify for mentions and keywords
    MentionsOnly,
    /// suppress all notifications
    Mute,
}

/// root configuration
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub notifications: NotificationConfig,
    pub behavior: BehaviorConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            notifications: NotificationConfig::default(),
            behavior: BehaviorConfig::default(),
        }
    }
}

/// notification filter settings
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    /// global enable/disable for this device
    pub enabled: bool,

    // === event type filters ===
    pub messages_one_to_one: NotifyLevel,
    pub messages_group: NotifyLevel,
    pub encrypted_one_to_one: NotifyLevel,
    pub encrypted_group: NotifyLevel,
    pub invites: NotifyLevel,
    pub membership_changes: NotifyLevel,
    pub room_upgrades: NotifyLevel,
    pub calls: NotifyLevel,
    pub reactions: NotifyLevel,
    pub edits: NotifyLevel,

    // === mention & keyword behavior ===
    pub mentions_user: NotifyLevel,
    pub mentions_room: NotifyLevel,
    pub mentions_display_name: NotifyLevel,
    pub mentions_keywords: NotifyLevel,

    /// custom keywords to watch for (evaluated locally on decrypted content)
    #[serde(default)]
    pub keywords: Vec<String>,

    /// if true, ignore all group chat notifications
    #[serde(default)]
    pub dms_only: bool,

    /// per-room notification overrides, keyed by room id
    #[serde(default)]
    pub rooms: HashMap<String, RoomNotifyLevel>,

    /// sender filters
    #[serde(default)]
    pub senders: SenderFilters,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            messages_one_to_one: NotifyLevel::Noisy,
            messages_group: NotifyLevel::Silent,
            encrypted_one_to_one: NotifyLevel::Noisy,
            encrypted_group: NotifyLevel::Silent,
            invites: NotifyLevel::Noisy,
            membership_changes: NotifyLevel::Off,
            room_upgrades: NotifyLevel::Silent,
            calls: NotifyLevel::Noisy,
            reactions: NotifyLevel::Off,
            edits: NotifyLevel::Off,
            mentions_user: NotifyLevel::Noisy,
            mentions_room: NotifyLevel::Silent,
            mentions_display_name: NotifyLevel::Noisy,
            mentions_keywords: NotifyLevel::Noisy,
            keywords: Vec::new(),
            dms_only: false,
            rooms: HashMap::new(),
            senders: SenderFilters::default(),
        }
    }
}

/// sender-based notification filters
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SenderFilters {
    /// always notify for these senders (mxids)
    pub always: Vec<String>,
    /// never notify for these senders (mxids)
    pub never: Vec<String>,
}

/// behavior tuning settings
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    /// suppress notifications for events older than this (seconds)
    pub max_event_age_secs: u64,

    /// suppress notifications if another session has shown a read receipt
    /// within this window (seconds)
    pub read_receipt_grace_period_secs: u64,

    /// show message body in notification
    pub show_message_body: bool,

    /// show reply context (e.g., "alice (to bob): ...")
    pub show_reply_context: bool,

    /// truncate message body to this many characters
    pub max_body_length: usize,

    /// group multiple messages from the same room into a single notification
    pub collapse_room_notifications: bool,

    /// command to run when a notification is clicked
    /// placeholders: {room_id}, {event_id}, {sender}
    pub on_click_command: Option<String>,

    /// quiet hours configuration
    #[serde(default)]
    pub quiet_hours: QuietHoursConfig,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            max_event_age_secs: 60,
            read_receipt_grace_period_secs: 5,
            show_message_body: true,
            show_reply_context: true,
            max_body_length: 300,
            collapse_room_notifications: true,
            on_click_command: None,
            quiet_hours: QuietHoursConfig::default(),
        }
    }
}

/// quiet hours (built-in do-not-disturb schedule)
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuietHoursConfig {
    pub enabled: bool,
    /// start time in 24h format (e.g., "23:00")
    pub start: String,
    /// end time in 24h format (e.g., "07:00")
    pub end: String,
    /// iana timezone string, or "local" for system timezone
    pub timezone: String,
}

impl Default for QuietHoursConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            start: "23:00".to_string(),
            end: "07:00".to_string(),
            timezone: "local".to_string(),
        }
    }
}

impl Config {
    /// load config from a toml file, returns default config if it doesn't exist
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::info!(?path, "Config file not found, using defaults");
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        tracing::info!(?path, "Config loaded");
        Ok(config)
    }

    /// resolve the config file path from cli flag, env var, or platform default
    pub fn resolve_config_path(cli_path: Option<&Path>) -> PathBuf {
        if let Some(path) = cli_path {
            return path.to_path_buf();
        }

        if let Ok(path) = std::env::var("PSST_CONFIG") {
            return PathBuf::from(path);
        }

        Self::default_config_path()
    }

    /// resolve the data directory from cli flag, env var, or platform default
    pub fn resolve_data_dir(cli_path: Option<&Path>) -> PathBuf {
        if let Some(path) = cli_path {
            return path.to_path_buf();
        }

        if let Ok(path) = std::env::var("PSST_DATA_DIR") {
            return PathBuf::from(path);
        }

        Self::default_data_dir()
    }

    fn default_config_path() -> PathBuf {
        directories::ProjectDirs::from("com", "csutora", "psst")
            .map(|dirs| dirs.config_dir().join("config.toml"))
            .unwrap_or_else(|| PathBuf::from("config.toml"))
    }

    fn default_data_dir() -> PathBuf {
        directories::ProjectDirs::from("com", "csutora", "psst")
            .map(|dirs| dirs.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}
