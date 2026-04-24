use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyLevel {
    #[default]
    Off,
    Silent,
    Noisy,
}

impl NotifyLevel {
    pub fn loudest(self, other: Self) -> Self {
        use NotifyLevel::*;
        match (self, other) {
            (Noisy, _) | (_, Noisy) => Noisy,
            (Silent, _) | (_, Silent) => Silent,
            _ => Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    Off,
    Silent,
    Noisy,
    #[default]
    Replace,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoomConfig {
    pub unencrypted: NotifyLevel,
    pub encrypted: NotifyLevel,
    pub mentions_you: NotifyLevel,
    pub mentions_room: NotifyLevel,
    pub keyword_match: NotifyLevel,
    pub calls: NotifyLevel,
    pub reactions: NotifyLevel,
    pub edits: EditMode,
    pub noisy_debounce_seconds: u64,
    pub keywords: Vec<String>,
}

impl RoomConfig {
    pub fn default_dms() -> Self {
        Self {
            unencrypted: NotifyLevel::Noisy,
            encrypted: NotifyLevel::Noisy,
            mentions_you: NotifyLevel::Noisy,
            mentions_room: NotifyLevel::Noisy,
            keyword_match: NotifyLevel::Noisy,
            calls: NotifyLevel::Noisy,
            reactions: NotifyLevel::Noisy,
            edits: EditMode::Replace,
            noisy_debounce_seconds: 0,
            keywords: Vec::new(),
        }
    }

    pub fn default_rooms() -> Self {
        Self {
            unencrypted: NotifyLevel::Silent,
            encrypted: NotifyLevel::Silent,
            mentions_you: NotifyLevel::Noisy,
            mentions_room: NotifyLevel::Silent,
            keyword_match: NotifyLevel::Noisy,
            calls: NotifyLevel::Noisy,
            reactions: NotifyLevel::Silent,
            edits: EditMode::Replace,
            noisy_debounce_seconds: 0,
            keywords: Vec::new(),
        }
    }

    fn muted() -> Self {
        Self {
            unencrypted: NotifyLevel::Off,
            encrypted: NotifyLevel::Off,
            mentions_you: NotifyLevel::Off,
            mentions_room: NotifyLevel::Off,
            keyword_match: NotifyLevel::Off,
            calls: NotifyLevel::Off,
            reactions: NotifyLevel::Off,
            edits: EditMode::Off,
            noisy_debounce_seconds: 0,
            keywords: Vec::new(),
        }
    }
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self::default_rooms()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoomOverride {
    pub mute: bool,
    pub unencrypted: Option<NotifyLevel>,
    pub encrypted: Option<NotifyLevel>,
    pub mentions_you: Option<NotifyLevel>,
    pub mentions_room: Option<NotifyLevel>,
    pub keyword_match: Option<NotifyLevel>,
    pub calls: Option<NotifyLevel>,
    pub reactions: Option<NotifyLevel>,
    pub edits: Option<EditMode>,
    pub noisy_debounce_seconds: Option<u64>,
    pub keywords: Option<Vec<String>>,
}

impl RoomOverride {
    fn apply_to(&self, mut base: RoomConfig) -> RoomConfig {
        if self.mute {
            return RoomConfig::muted();
        }
        if let Some(v) = self.unencrypted {
            base.unencrypted = v;
        }
        if let Some(v) = self.encrypted {
            base.encrypted = v;
        }
        if let Some(v) = self.mentions_you {
            base.mentions_you = v;
        }
        if let Some(v) = self.mentions_room {
            base.mentions_room = v;
        }
        if let Some(v) = self.keyword_match {
            base.keyword_match = v;
        }
        if let Some(v) = self.calls {
            base.calls = v;
        }
        if let Some(v) = self.reactions {
            base.reactions = v;
        }
        if let Some(v) = self.edits {
            base.edits = v;
        }
        if let Some(v) = self.noisy_debounce_seconds {
            base.noisy_debounce_seconds = v;
        }
        if let Some(v) = self.keywords.clone() {
            base.keywords = v;
        }
        base
    }
}

/// rooms section. raw deserialization captures unknown keys (overrides + typos).
/// after parsing, we walk extras: keys starting with `!` become overrides, others log a warning.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawRoomsSection {
    #[serde(flatten)]
    pub extras: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct RoomsSection {
    pub defaults: RoomConfig,
    pub overrides: HashMap<String, RoomOverride>,
}

impl RoomsSection {
    fn from_raw(raw: RawRoomsSection, default: RoomConfig, label: &str) -> Self {
        // partition extras into known fields vs override candidates
        let mut defaults_table = toml::Table::new();
        let mut override_candidates: Vec<(String, toml::Value)> = Vec::new();

        let known_fields: &[&str] = &[
            "unencrypted",
            "encrypted",
            "mentions_you",
            "mentions_room",
            "keyword_match",
            "calls",
            "reactions",
            "edits",
            "noisy_debounce_seconds",
            "keywords",
        ];

        for (key, value) in raw.extras {
            if known_fields.contains(&key.as_str()) {
                defaults_table.insert(key, value);
            } else if key.starts_with('!') {
                override_candidates.push((key, value));
            } else {
                tracing::warn!(
                    "ignoring unknown field `{key}` in [notifications.{label}] (room overrides must start with `!`)"
                );
            }
        }

        let defaults: RoomConfig = match toml::Value::Table(defaults_table).try_into() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "ignoring invalid defaults in [notifications.{label}]: {e}. using defaults."
                );
                default.clone()
            }
        };

        let mut overrides: HashMap<String, RoomOverride> = HashMap::new();
        for (key, value) in override_candidates {
            match value.try_into::<RoomOverride>() {
                Ok(ov) => {
                    overrides.insert(key, ov);
                }
                Err(e) => {
                    tracing::warn!(
                        "ignoring invalid override [notifications.{label}.\"{key}\"]: {e}"
                    );
                }
            }
        }

        Self { defaults, overrides }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SenderFilters {
    pub noisy: Vec<String>,
    pub silent: Vec<String>,
    pub off: Vec<String>,
}

impl SenderFilters {
    /// is this sender in any of the noisy/silent/off lists?
    /// used to bypass per-room debounce/quiet-hours for explicitly-listed senders.
    pub fn contains(&self, sender: &str) -> bool {
        self.noisy.iter().any(|x| x == sender)
            || self.silent.iter().any(|x| x == sender)
            || self.off.iter().any(|x| x == sender)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RawNotificationConfig {
    pub enabled: bool,
    pub sound: String,
    pub invites: NotifyLevel,
    pub dms: RoomOverride,
    pub rooms: RawRoomsSection,
    pub senders: SenderFilters,
}

impl Default for RawNotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sound: default_sound(),
            invites: NotifyLevel::Noisy,
            dms: RoomOverride::default(),
            rooms: RawRoomsSection::default(),
            senders: SenderFilters::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub sound: String,
    pub invites: NotifyLevel,
    pub dms: RoomConfig,
    pub rooms: RoomsSection,
    pub senders: SenderFilters,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sound: default_sound(),
            invites: NotifyLevel::Noisy,
            dms: RoomConfig::default_dms(),
            rooms: RoomsSection::default(),
            senders: SenderFilters::default(),
        }
    }
}

impl NotificationConfig {
    /// resolve the effective room config for a given room, applying any per-room override
    pub fn effective_for(&self, room_id: &str, is_direct: bool) -> RoomConfig {
        let base = if is_direct {
            self.dms.clone()
        } else {
            self.rooms.defaults.clone()
        };
        match self.rooms.overrides.get(room_id) {
            Some(ov) => ov.apply_to(base),
            None => base,
        }
    }
}

fn default_sound() -> String {
    if cfg!(target_os = "macos") {
        "Blow".to_string()
    } else {
        "message-new-instant".to_string()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BehaviorConfig {
    pub show_message_body: bool,
    pub max_body_length: usize,
    pub max_event_age_secs: u64,
    pub quiet_hours: QuietHoursConfig,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            show_message_body: true,
            max_body_length: 300,
            max_event_age_secs: 60,
            quiet_hours: QuietHoursConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuietHoursConfig {
    pub enabled: bool,
    pub start: String,
    pub end: String,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    pub notifications: RawNotificationConfig,
    pub behavior: BehaviorConfig,
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub notifications: NotificationConfig,
    pub behavior: BehaviorConfig,
}

impl Config {
    /// load config from a toml file, returns default config if it doesn't exist
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::info!(?path, "config file not found, using defaults");
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;

        let de = toml::Deserializer::parse(&content)
            .with_context(|| format!("failed to parse config file: {}", path.display()))?;
        let raw: RawConfig = serde_ignored::deserialize(de, |path| {
            tracing::warn!("ignoring unknown config field `{path}`");
        })
        .with_context(|| format!("failed to parse config file: {}", path.display()))?;

        let config = Self {
            notifications: NotificationConfig {
                enabled: raw.notifications.enabled,
                sound: raw.notifications.sound,
                invites: raw.notifications.invites,
                dms: raw.notifications.dms.apply_to(RoomConfig::default_dms()),
                rooms: RoomsSection::from_raw(
                    raw.notifications.rooms,
                    RoomConfig::default_rooms(),
                    "rooms",
                ),
                senders: raw.notifications.senders,
            },
            behavior: raw.behavior,
        };

        tracing::info!(?path, "config loaded");
        Ok(config)
    }

    pub fn resolve_config_path(cli_path: Option<&Path>) -> PathBuf {
        if let Some(path) = cli_path {
            return path.to_path_buf();
        }
        if let Ok(path) = std::env::var("PSST_CONFIG") {
            return PathBuf::from(path);
        }
        Self::default_config_path()
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Config {
        let de = toml::Deserializer::parse(content).unwrap();
        let raw: RawConfig = serde_ignored::deserialize(de, |_| {}).unwrap();
        Config {
            notifications: NotificationConfig {
                enabled: raw.notifications.enabled,
                sound: raw.notifications.sound,
                invites: raw.notifications.invites,
                dms: raw.notifications.dms.apply_to(RoomConfig::default_dms()),
                rooms: RoomsSection::from_raw(
                    raw.notifications.rooms,
                    RoomConfig::default_rooms(),
                    "rooms",
                ),
                senders: raw.notifications.senders,
            },
            behavior: raw.behavior,
        }
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert!(c.notifications.enabled);
        assert_eq!(c.notifications.invites, NotifyLevel::Noisy);
        assert_eq!(c.notifications.dms.unencrypted, NotifyLevel::Noisy);
        assert_eq!(c.notifications.rooms.defaults.unencrypted, NotifyLevel::Silent);
        assert_eq!(c.behavior.max_body_length, 300);
    }

    #[test]
    fn parse_minimal() {
        let c = parse("");
        assert!(c.notifications.enabled);
    }

    #[test]
    fn parse_partial_dms() {
        let c = parse(
            r#"
            [notifications.dms]
            unencrypted = "off"
            "#,
        );
        assert_eq!(c.notifications.dms.unencrypted, NotifyLevel::Off);
        assert_eq!(c.notifications.dms.encrypted, NotifyLevel::Noisy);
    }

    #[test]
    fn parse_room_override() {
        let c = parse(
            r#"
            [notifications.rooms."!abc:example.com"]
            unencrypted = "noisy"
            edits = "off"
            "#,
        );
        let eff = c.notifications.effective_for("!abc:example.com", false);
        assert_eq!(eff.unencrypted, NotifyLevel::Noisy);
        assert_eq!(eff.edits, EditMode::Off);
        assert_eq!(eff.encrypted, NotifyLevel::Silent);
    }

    #[test]
    fn parse_room_mute() {
        let c = parse(
            r#"
            [notifications.rooms."!abc:example.com"]
            mute = true
            "#,
        );
        let eff = c.notifications.effective_for("!abc:example.com", false);
        assert_eq!(eff.unencrypted, NotifyLevel::Off);
        assert_eq!(eff.mentions_you, NotifyLevel::Off);
        assert_eq!(eff.edits, EditMode::Off);
    }

    #[test]
    fn dm_uses_dm_defaults_with_override() {
        let c = parse(
            r#"
            [notifications.rooms."!dm:example.com"]
            edits = "off"
            "#,
        );
        let eff = c.notifications.effective_for("!dm:example.com", true);
        // dm defaults
        assert_eq!(eff.unencrypted, NotifyLevel::Noisy);
        // overridden
        assert_eq!(eff.edits, EditMode::Off);
    }

    #[test]
    fn parse_senders() {
        let c = parse(
            r#"
            [notifications.senders]
            noisy = ["@vip:example.com"]
            silent = ["@neutral:example.com"]
            off = ["@bot:example.com"]
            "#,
        );
        assert_eq!(c.notifications.senders.noisy, vec!["@vip:example.com"]);
        assert_eq!(c.notifications.senders.silent, vec!["@neutral:example.com"]);
        assert_eq!(c.notifications.senders.off, vec!["@bot:example.com"]);
    }

    #[test]
    fn parse_quiet_hours() {
        let c = parse(
            r#"
            [behavior.quiet_hours]
            enabled = true
            start = "22:00"
            end = "08:00"
            "#,
        );
        assert!(c.behavior.quiet_hours.enabled);
        assert_eq!(c.behavior.quiet_hours.start, "22:00");
    }

    #[test]
    fn loudest_picks_louder() {
        assert_eq!(NotifyLevel::Off.loudest(NotifyLevel::Silent), NotifyLevel::Silent);
        assert_eq!(NotifyLevel::Silent.loudest(NotifyLevel::Noisy), NotifyLevel::Noisy);
        assert_eq!(NotifyLevel::Noisy.loudest(NotifyLevel::Off), NotifyLevel::Noisy);
    }

    #[test]
    fn load_missing_file() {
        let c = Config::load(Path::new("/nonexistent/path/config.toml")).unwrap();
        assert!(c.notifications.enabled);
    }

    #[test]
    fn invalid_toml_errors() {
        let invalid = "not = valid = toml";
        let result = toml::Deserializer::parse(invalid);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_top_level_field_calls_ignored_callback() {
        let toml_text = r#"
            [notifications]
            enabled = true
            mystery_field = "huh"

            [behavior]
            show_message_body = true
        "#;
        let de = toml::Deserializer::parse(toml_text).unwrap();
        let mut paths: Vec<String> = Vec::new();
        let _: RawConfig = serde_ignored::deserialize(de, |p| paths.push(p.to_string())).unwrap();
        assert!(
            paths.iter().any(|p| p.contains("mystery_field")),
            "expected unknown field path captured, got: {paths:?}"
        );
    }

    #[test]
    fn invalid_enum_variant_returns_err() {
        let toml_text = r#"
            [notifications.dms]
            unencrypted = "noise"
        "#;
        let de = toml::Deserializer::parse(toml_text).unwrap();
        let result: Result<RawConfig, _> = serde_ignored::deserialize(de, |_| {});
        let err = result.expect_err("expected error for invalid enum variant");
        let msg = err.to_string();
        // serde error mentions the unknown variant or expected variants
        assert!(
            msg.contains("noise") || msg.contains("variant") || msg.contains("expected"),
            "unhelpful error: {msg}"
        );
    }

    #[test]
    fn override_key_without_bang_dropped() {
        // typo: "unencryptd" instead of "unencrypted" → not a known field, doesn't start with !
        // expected: dropped (warning logged), no override created, defaults preserved
        let c = parse(
            r#"
            [notifications.rooms]
            unencryptd = "noisy"
            "#,
        );
        assert!(c.notifications.rooms.overrides.is_empty());
        // defaults still apply
        assert_eq!(c.notifications.rooms.defaults.unencrypted, NotifyLevel::Silent);
    }

    #[test]
    fn override_with_invalid_inner_field_dropped() {
        // valid override key, but invalid inner field name → override dropped
        let c = parse(
            r#"
            [notifications.rooms."!a:b"]
            this_is_not_a_field = "noisy"
            "#,
        );
        assert!(c.notifications.rooms.overrides.is_empty());
    }

    #[test]
    fn effective_for_with_mute_returns_all_off() {
        let c = parse(
            r#"
            [notifications.rooms."!quiet:example.com"]
            mute = true
            "#,
        );
        let eff = c.notifications.effective_for("!quiet:example.com", false);
        assert_eq!(eff.unencrypted, NotifyLevel::Off);
        assert_eq!(eff.encrypted, NotifyLevel::Off);
        assert_eq!(eff.mentions_you, NotifyLevel::Off);
        assert_eq!(eff.mentions_room, NotifyLevel::Off);
        assert_eq!(eff.keyword_match, NotifyLevel::Off);
        assert_eq!(eff.calls, NotifyLevel::Off);
        assert_eq!(eff.reactions, NotifyLevel::Off);
        assert_eq!(eff.edits, EditMode::Off);
    }

    #[test]
    fn load_real_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
                [notifications]
                enabled = true
                sound = "Tink"
                invites = "silent"

                [notifications.dms]
                unencrypted = "off"

                [notifications.rooms."!special:example.com"]
                unencrypted = "noisy"

                [notifications.senders]
                noisy = ["@vip:example.com"]

                [behavior]
                max_body_length = 100
            "#,
        )
        .unwrap();

        let c = Config::load(&path).unwrap();
        assert!(c.notifications.enabled);
        assert_eq!(c.notifications.sound, "Tink");
        assert_eq!(c.notifications.invites, NotifyLevel::Silent);
        assert_eq!(c.notifications.dms.unencrypted, NotifyLevel::Off);
        // dms default for other fields preserved
        assert_eq!(c.notifications.dms.encrypted, NotifyLevel::Noisy);
        assert!(c.notifications.rooms.overrides.contains_key("!special:example.com"));
        let eff = c.notifications.effective_for("!special:example.com", false);
        assert_eq!(eff.unencrypted, NotifyLevel::Noisy);
        assert_eq!(c.notifications.senders.noisy, vec!["@vip:example.com"]);
        assert_eq!(c.behavior.max_body_length, 100);
    }

    fn senders(noisy: &[&str], silent: &[&str], off: &[&str]) -> SenderFilters {
        SenderFilters {
            noisy: noisy.iter().map(|s| s.to_string()).collect(),
            silent: silent.iter().map(|s| s.to_string()).collect(),
            off: off.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn sender_filters_contains_finds_in_each_list() {
        let s = senders(&["@a:x"], &["@b:x"], &["@c:x"]);
        assert!(s.contains("@a:x"));
        assert!(s.contains("@b:x"));
        assert!(s.contains("@c:x"));
    }

    #[test]
    fn sender_filters_contains_returns_false_when_not_listed() {
        let s = senders(&["@a:x"], &["@b:x"], &["@c:x"]);
        assert!(!s.contains("@unknown:x"));
    }
}
