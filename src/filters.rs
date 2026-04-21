use matrix_sdk::ruma::events::room::message::MessageType;

use crate::config::{Config, NotifyLevel, RoomNotifyLevel};

/// result of evaluating the filter pipeline
pub enum FilterResult {
    /// send a notification
    Notify { sound: bool },
    /// suppress this event
    Suppress,
}

/// context about an event, extracted before filter evaluation
pub struct EventContext {
    pub sender: String,
    pub room_id: String,
    pub is_direct: bool,
    pub is_encrypted: bool,
    pub is_edit: bool,
    pub push_notify: bool,
    pub push_highlight: bool,
    pub event_ts_secs: i64,
}

/// evaluate the filter pipeline for an event
pub fn evaluate(ctx: &EventContext, config: &Config) -> FilterResult {
    // 1. skip edits (never re-notify)
    if ctx.is_edit {
        return FilterResult::Suppress;
    }

    // 2. global enable
    if !config.notifications.enabled {
        return FilterResult::Suppress;
    }

    // 3. quiet hours
    if is_quiet_hours(config) {
        return FilterResult::Suppress;
    }

    // 4. sender never
    if config.notifications.senders.never.contains(&ctx.sender) {
        return FilterResult::Suppress;
    }

    // 5. sender always (overrides other filters)
    if config.notifications.senders.always.contains(&ctx.sender) {
        return FilterResult::Notify { sound: true };
    }

    // 6. room override
    if let Some(level) = config.notifications.rooms.get(&ctx.room_id) {
        match level {
            RoomNotifyLevel::Mute => return FilterResult::Suppress,
            RoomNotifyLevel::All => {}
            RoomNotifyLevel::MentionsOnly => {
                if !ctx.push_highlight {
                    return FilterResult::Suppress;
                }
            }
        }
    }

    // 7. direct messages only
    if config.notifications.dms_only && !ctx.is_direct {
        return FilterResult::Suppress;
    }

    // 8. push rules (from server)
    // for encrypted rooms, the server can't evaluate content-based push rules,
    // so the sdk may not provide Notify actions for decrypted events
    if !ctx.push_notify && !ctx.is_encrypted {
        return FilterResult::Suppress;
    }

    // 9. event type level
    let level = message_level(ctx, config);
    if matches!(level, NotifyLevel::Off) {
        return FilterResult::Suppress;
    }

    // 10. max event age
    let now = chrono::Utc::now().timestamp();
    let age = now - ctx.event_ts_secs;
    if age > config.behavior.max_event_age_secs as i64 {
        return FilterResult::Suppress;
    }
    let sound = ctx.push_highlight || matches!(level, NotifyLevel::Noisy);
    FilterResult::Notify { sound }
}

/// determine the notification level for a message based on room type
fn message_level(ctx: &EventContext, config: &Config) -> NotifyLevel {
    let n = &config.notifications;
    match (ctx.is_encrypted, ctx.is_direct) {
        (true, true) => n.encrypted_one_to_one,
        (true, false) => n.encrypted_group,
        (false, true) => n.messages_one_to_one,
        (false, false) => n.messages_group,
    }
}

/// check if current time falls within quiet hours
fn is_quiet_hours(config: &Config) -> bool {
    let qh = &config.behavior.quiet_hours;
    if !qh.enabled {
        return false;
    }

    let now = chrono::Local::now().time();

    let start = match chrono::NaiveTime::parse_from_str(&qh.start, "%H:%M") {
        Ok(t) => t,
        Err(_) => return false,
    };
    let end = match chrono::NaiveTime::parse_from_str(&qh.end, "%H:%M") {
        Ok(t) => t,
        Err(_) => return false,
    };

    if start <= end {
        // same-day range (e.g., 09:00-17:00)
        now >= start && now < end
    } else {
        // overnight range (e.g., 23:00-07:00)
        now >= start || now < end
    }
}

/// format the message body for display in a notification
pub fn format_body(msgtype: &MessageType, sender: &str, config: &Config) -> String {
    if !config.behavior.show_message_body {
        return "new message".to_string();
    }

    let body = match msgtype {
        MessageType::Text(c) => c.body.clone(),
        MessageType::Notice(c) => c.body.clone(),
        MessageType::Emote(c) => format!("* {sender} {}", c.body),
        MessageType::Image(_) => "[image]".to_string(),
        MessageType::Video(_) => "[video]".to_string(),
        MessageType::Audio(_) => "[audio]".to_string(),
        MessageType::File(c) => format!("[file: {}]", c.body),
        _ => "new message".to_string(),
    };

    truncate(&body, config.behavior.max_body_length)
}

/// utf-8 safe truncation with ellipsis
fn truncate(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}...", &s[..end])
    }
}
