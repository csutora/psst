use std::collections::BTreeSet;

use matrix_sdk::ruma::events::room::message::MessageType;
use matrix_sdk::ruma::{OwnedUserId, UserId};

use crate::config::{Config, EditMode, NotifyLevel, RoomConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Message,
    Invite,
    Call,
    Reaction,
    Edit,
}

#[derive(Debug, PartialEq)]
pub enum FilterResult {
    Notify { sound: bool },
    Replace,
    Suppress,
}

pub struct EventContext {
    pub kind: EventKind,
    pub sender: String,
    pub room_id: String,
    pub is_direct: bool,
    pub is_encrypted: bool,
    pub mentions_you: bool,
    pub mentions_room: bool,
    pub matched_keyword: bool,
    pub event_ts_secs: i64,
}

pub fn evaluate(
    ctx: &EventContext,
    config: &Config,
    now: chrono::DateTime<chrono::Local>,
) -> FilterResult {
    let age = now.timestamp() - ctx.event_ts_secs;
    if age > config.behavior.max_event_age_secs as i64 {
        return FilterResult::Suppress;
    }

    if !config.notifications.enabled {
        return FilterResult::Suppress;
    }

    let senders = &config.notifications.senders;
    if senders.off.iter().any(|s| s == &ctx.sender) {
        return FilterResult::Suppress;
    }
    if senders.silent.iter().any(|s| s == &ctx.sender) {
        return FilterResult::Notify { sound: false };
    }
    if senders.noisy.iter().any(|s| s == &ctx.sender) {
        return FilterResult::Notify { sound: true };
    }

    if is_quiet_hours(config, now.time()) {
        return FilterResult::Suppress;
    }

    let effective = config.notifications.effective_for(&ctx.room_id, ctx.is_direct);

    let level = match ctx.kind {
        EventKind::Invite => config.notifications.invites,
        EventKind::Call => effective.calls,
        EventKind::Reaction => effective.reactions,
        EventKind::Edit => match effective.edits {
            EditMode::Off => return FilterResult::Suppress,
            EditMode::Replace => return FilterResult::Replace,
            EditMode::Silent => NotifyLevel::Silent,
            EditMode::Noisy => NotifyLevel::Noisy,
        },
        EventKind::Message => message_level(ctx, &effective),
    };

    match level {
        NotifyLevel::Off => FilterResult::Suppress,
        NotifyLevel::Silent => FilterResult::Notify { sound: false },
        NotifyLevel::Noisy => FilterResult::Notify { sound: true },
    }
}

/// resolve the message level taking mentions/keywords into account; loudest of all matching wins
fn message_level(ctx: &EventContext, effective: &RoomConfig) -> NotifyLevel {
    let base = if ctx.is_encrypted {
        effective.encrypted
    } else {
        effective.unencrypted
    };
    let mut level = base;
    if ctx.mentions_you {
        level = level.loudest(effective.mentions_you);
    }
    if ctx.mentions_room {
        level = level.loudest(effective.mentions_room);
    }
    if ctx.matched_keyword {
        level = level.loudest(effective.keyword_match);
    }
    level
}

fn is_quiet_hours(config: &Config, now: chrono::NaiveTime) -> bool {
    let qh = &config.behavior.quiet_hours;
    if !qh.enabled {
        return false;
    }

    let start = match chrono::NaiveTime::parse_from_str(&qh.start, "%H:%M") {
        Ok(t) => t,
        Err(_) => return false,
    };
    let end = match chrono::NaiveTime::parse_from_str(&qh.end, "%H:%M") {
        Ok(t) => t,
        Err(_) => return false,
    };

    if start <= end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

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

/// detect mention/keyword highlights for a message body.
/// `mentions` is `Some((user_ids, room))` if the sender's client set m.mentions (MSC3952);
/// when the field is present we trust it and skip body parsing entirely.
/// when None, fall back to substring parsing for legacy clients.
pub fn detect_highlights(
    body: &str,
    mentions: Option<(&BTreeSet<OwnedUserId>, bool)>,
    own_user_id: &UserId,
    display_name: Option<&str>,
    keywords: &[String],
) -> (bool, bool, bool) {
    let (mentions_you, mentions_room) = match mentions {
        Some((user_ids, room)) => (user_ids.contains(own_user_id), room),
        None => detect_mentions_from_body(body, own_user_id, display_name),
    };

    let lower_body = body.to_lowercase();
    let matched_keyword = !keywords.is_empty()
        && keywords.iter().any(|kw| lower_body.contains(&kw.to_lowercase()));

    (mentions_you, mentions_room, matched_keyword)
}

/// fallback for legacy clients that don't set m.mentions
fn detect_mentions_from_body(
    body: &str,
    own_user_id: &UserId,
    display_name: Option<&str>,
) -> (bool, bool) {
    let lower = body.to_lowercase();
    let localpart_pat = format!("@{}", own_user_id.localpart()).to_lowercase();
    let mxid_pat = own_user_id.as_str().to_lowercase();
    let by_id = lower.contains(&localpart_pat) || lower.contains(&mxid_pat);
    let by_name = display_name
        .filter(|dn| !dn.is_empty())
        .is_some_and(|dn| lower.contains(&dn.to_lowercase()));
    let mentions_room = lower.contains("@room");
    (by_id || by_name, mentions_room)
}

pub fn truncate(s: &str, max_chars: usize) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn now_ts() -> i64 {
        chrono::Utc::now().timestamp()
    }

    fn ctx() -> EventContext {
        EventContext {
            kind: EventKind::Message,
            sender: "@alice:example.com".into(),
            room_id: "!room:example.com".into(),
            is_direct: true,
            is_encrypted: false,
            mentions_you: false,
            mentions_room: false,
            matched_keyword: false,
            event_ts_secs: now_ts(),
        }
    }

    #[test]
    fn dm_message_default_noisy() {
        assert_eq!(
            evaluate(&ctx(), &Config::default(), chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    #[test]
    fn group_message_default_silent() {
        let c = EventContext { is_direct: false, ..ctx() };
        assert_eq!(
            evaluate(&c, &Config::default(), chrono::Local::now()),
            FilterResult::Notify { sound: false }
        );
    }

    #[test]
    fn old_event_suppressed() {
        let c = EventContext { event_ts_secs: now_ts() - 3600, ..ctx() };
        assert_eq!(evaluate(&c, &Config::default(), chrono::Local::now()), FilterResult::Suppress);
    }

    #[test]
    fn disabled_globally() {
        let mut config = Config::default();
        config.notifications.enabled = false;
        assert_eq!(evaluate(&ctx(), &config, chrono::Local::now()), FilterResult::Suppress);
    }

    #[test]
    fn sender_off_suppresses() {
        let mut config = Config::default();
        config.notifications.senders.off.push("@alice:example.com".into());
        assert_eq!(evaluate(&ctx(), &config, chrono::Local::now()), FilterResult::Suppress);
    }

    #[test]
    fn sender_silent_overrides_dm_noisy() {
        let mut config = Config::default();
        config.notifications.senders.silent.push("@alice:example.com".into());
        assert_eq!(
            evaluate(&ctx(), &config, chrono::Local::now()),
            FilterResult::Notify { sound: false }
        );
    }

    #[test]
    fn sender_noisy_overrides_room_silent() {
        let mut config = Config::default();
        config.notifications.senders.noisy.push("@alice:example.com".into());
        let c = EventContext { is_direct: false, ..ctx() };
        assert_eq!(
            evaluate(&c, &config, chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    #[test]
    fn mention_you_upgrades_silent_room_to_noisy() {
        let c = EventContext {
            is_direct: false,
            mentions_you: true,
            ..ctx()
        };
        assert_eq!(
            evaluate(&c, &Config::default(), chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    #[test]
    fn keyword_match_upgrades_silent_room() {
        let c = EventContext {
            is_direct: false,
            matched_keyword: true,
            ..ctx()
        };
        assert_eq!(
            evaluate(&c, &Config::default(), chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    #[test]
    fn invite_uses_root_invites_level() {
        let c = EventContext { kind: EventKind::Invite, ..ctx() };
        assert_eq!(
            evaluate(&c, &Config::default(), chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    #[test]
    fn reaction_dm_default_noisy() {
        let c = EventContext { kind: EventKind::Reaction, ..ctx() };
        assert_eq!(
            evaluate(&c, &Config::default(), chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    #[test]
    fn reaction_room_default_silent() {
        let c = EventContext {
            kind: EventKind::Reaction,
            is_direct: false,
            ..ctx()
        };
        assert_eq!(
            evaluate(&c, &Config::default(), chrono::Local::now()),
            FilterResult::Notify { sound: false }
        );
    }

    #[test]
    fn edit_default_replace() {
        let c = EventContext { kind: EventKind::Edit, ..ctx() };
        assert_eq!(evaluate(&c, &Config::default(), chrono::Local::now()), FilterResult::Replace);
    }

    #[test]
    fn edit_off_suppresses() {
        let mut config = Config::default();
        config.notifications.dms.edits = EditMode::Off;
        let c = EventContext { kind: EventKind::Edit, ..ctx() };
        assert_eq!(evaluate(&c, &config, chrono::Local::now()), FilterResult::Suppress);
    }

    #[test]
    fn edit_silent() {
        let mut config = Config::default();
        config.notifications.dms.edits = EditMode::Silent;
        let c = EventContext { kind: EventKind::Edit, ..ctx() };
        assert_eq!(
            evaluate(&c, &config, chrono::Local::now()),
            FilterResult::Notify { sound: false }
        );
    }

    #[test]
    fn edit_noisy() {
        let mut config = Config::default();
        config.notifications.dms.edits = EditMode::Noisy;
        let c = EventContext { kind: EventKind::Edit, ..ctx() };
        assert_eq!(
            evaluate(&c, &config, chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    // --- quiet hours ---

    /// build a deterministic local datetime for tests; the date doesn't matter,
    /// only the wall-clock time-of-day, but `event_ts_secs` must be near `now`
    /// to avoid the max_event_age check tripping.
    fn local_at(hour: u32, minute: u32) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(2026, 1, 1, hour, minute, 0)
            .single()
            .expect("constructed local time should be unambiguous")
    }

    /// produce a ctx whose event_ts matches the given `now`, so age checks pass
    fn ctx_at(now: chrono::DateTime<chrono::Local>) -> EventContext {
        EventContext { event_ts_secs: now.timestamp(), ..ctx() }
    }

    #[test]
    fn quiet_hours_same_day_inside_window_suppresses() {
        let mut config = Config::default();
        config.behavior.quiet_hours.enabled = true;
        config.behavior.quiet_hours.start = "22:00".into();
        config.behavior.quiet_hours.end = "23:00".into();
        let now = local_at(22, 30);
        assert_eq!(evaluate(&ctx_at(now), &config, now), FilterResult::Suppress);
    }

    #[test]
    fn quiet_hours_same_day_outside_window_notifies() {
        let mut config = Config::default();
        config.behavior.quiet_hours.enabled = true;
        config.behavior.quiet_hours.start = "22:00".into();
        config.behavior.quiet_hours.end = "23:00".into();
        let now = local_at(12, 0);
        assert_eq!(
            evaluate(&ctx_at(now), &config, now),
            FilterResult::Notify { sound: true }
        );
    }

    #[test]
    fn quiet_hours_overnight_inside_window_suppresses() {
        // overnight 22:00 → 06:00 ; with start > end, in-window means now >= start || now < end
        let mut config = Config::default();
        config.behavior.quiet_hours.enabled = true;
        config.behavior.quiet_hours.start = "22:00".into();
        config.behavior.quiet_hours.end = "06:00".into();

        // late evening → in window
        let late = local_at(23, 0);
        assert_eq!(evaluate(&ctx_at(late), &config, late), FilterResult::Suppress);

        // early morning → also in window (wraps past midnight)
        let early = local_at(2, 0);
        assert_eq!(evaluate(&ctx_at(early), &config, early), FilterResult::Suppress);
    }

    #[test]
    fn quiet_hours_overnight_outside_window_notifies() {
        let mut config = Config::default();
        config.behavior.quiet_hours.enabled = true;
        config.behavior.quiet_hours.start = "22:00".into();
        config.behavior.quiet_hours.end = "06:00".into();
        let now = local_at(12, 0);
        assert_eq!(
            evaluate(&ctx_at(now), &config, now),
            FilterResult::Notify { sound: true }
        );
    }

    #[test]
    fn sender_noisy_bypasses_quiet_hours() {
        let mut config = Config::default();
        config.behavior.quiet_hours.enabled = true;
        config.behavior.quiet_hours.start = "00:00".into();
        config.behavior.quiet_hours.end = "23:59".into();
        config.notifications.senders.noisy.push("@alice:example.com".into());
        // even within quiet hours, sender_noisy → notify with sound
        assert_eq!(
            evaluate(&ctx(), &config, chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    // --- mute ---

    #[test]
    fn mute_suppresses_message_reaction_call_edit() {
        let mut config = Config::default();
        let ov = crate::config::RoomOverride { mute: true, ..Default::default() };
        config
            .notifications
            .rooms
            .overrides
            .insert("!room:example.com".into(), ov);

        // group room (effective_for uses rooms.defaults + override)
        for kind in [EventKind::Message, EventKind::Reaction, EventKind::Call, EventKind::Edit] {
            let c = EventContext { kind, is_direct: false, ..ctx() };
            assert_eq!(
                evaluate(&c, &config, chrono::Local::now()),
                FilterResult::Suppress,
                "kind {:?} should be muted",
                kind
            );
        }
    }

    #[test]
    fn mute_does_not_suppress_invite() {
        // invites use the root config.notifications.invites level, not effective room config
        let mut config = Config::default();
        let ov = crate::config::RoomOverride { mute: true, ..Default::default() };
        config
            .notifications
            .rooms
            .overrides
            .insert("!room:example.com".into(), ov);
        let c = EventContext { kind: EventKind::Invite, is_direct: false, ..ctx() };
        assert_eq!(
            evaluate(&c, &config, chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    // --- call levels ---

    #[test]
    fn call_default_levels() {
        // dm: noisy
        let c = EventContext { kind: EventKind::Call, is_direct: true, ..ctx() };
        assert_eq!(
            evaluate(&c, &Config::default(), chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
        // room: noisy by default
        let c = EventContext { kind: EventKind::Call, is_direct: false, ..ctx() };
        assert_eq!(
            evaluate(&c, &Config::default(), chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    // --- loudest wins ---

    #[test]
    fn mention_and_keyword_loudest_wins() {
        let mut config = Config::default();
        // group room defaults: mentions_you = noisy, keyword_match = noisy
        // make mentions_you silent, keyword_match noisy → loudest = noisy
        config.notifications.rooms.defaults.mentions_you = NotifyLevel::Silent;
        config.notifications.rooms.defaults.keyword_match = NotifyLevel::Noisy;
        let c = EventContext {
            is_direct: false,
            mentions_you: true,
            matched_keyword: true,
            ..ctx()
        };
        assert_eq!(
            evaluate(&c, &config, chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }

    // --- detect_highlights ---

    use matrix_sdk::ruma::user_id;

    fn me() -> OwnedUserId {
        user_id!("@me:example.com").to_owned()
    }

    #[test]
    fn mentions_field_user_id_match() {
        let mut user_ids = BTreeSet::new();
        user_ids.insert(me());
        let (you, room, _) = detect_highlights(
            "hey",
            Some((&user_ids, false)),
            &me(),
            None,
            &[],
        );
        assert!(you);
        assert!(!room);
    }

    #[test]
    fn mentions_field_room_flag() {
        let user_ids = BTreeSet::new();
        let (you, room, _) = detect_highlights(
            "everyone read this",
            Some((&user_ids, true)),
            &me(),
            None,
            &[],
        );
        assert!(!you);
        assert!(room);
    }

    #[test]
    fn mentions_field_takes_precedence_over_body() {
        // body has @me but mentions field is empty. trust the sender's intent
        let user_ids = BTreeSet::new();
        let (you, _, _) = detect_highlights(
            "@me you should see this",
            Some((&user_ids, false)),
            &me(),
            None,
            &[],
        );
        assert!(!you);
    }

    #[test]
    fn body_fallback_localpart() {
        let (you, _, _) = detect_highlights(
            "hey @me what do you think",
            None,
            &me(),
            None,
            &[],
        );
        assert!(you);
    }

    #[test]
    fn body_fallback_full_mxid() {
        let (you, _, _) = detect_highlights(
            "ping @me:example.com please",
            None,
            &me(),
            None,
            &[],
        );
        assert!(you);
    }

    #[test]
    fn body_fallback_display_name() {
        let (you, _, _) = detect_highlights(
            "hey nara, take a look",
            None,
            &me(),
            Some("nara"),
            &[],
        );
        assert!(you);
    }

    #[test]
    fn body_fallback_room() {
        let (_, room, _) = detect_highlights(
            "@room standup in 5",
            None,
            &me(),
            None,
            &[],
        );
        assert!(room);
    }

    #[test]
    fn body_fallback_case_insensitive() {
        let (you, _, _) = detect_highlights(
            "HEY @ME WHAT IS UP",
            None,
            &me(),
            None,
            &[],
        );
        assert!(you);
    }

    #[test]
    fn keywords_substring_case_insensitive() {
        let kws = vec!["urgent".to_string(), "boss".to_string()];

        let (_, _, kw) = detect_highlights("This is URGENT please", None, &me(), None, &kws);
        assert!(kw);

        let (_, _, kw) = detect_highlights("the Boss is here", None, &me(), None, &kws);
        assert!(kw);

        let (_, _, kw) = detect_highlights("nothing of note", None, &me(), None, &kws);
        assert!(!kw);

        let (_, _, kw) = detect_highlights("urgent", None, &me(), None, &[]);
        assert!(!kw);
    }

    // --- room override changes message level ---

    #[test]
    fn room_override_changes_message_level() {
        let mut config = Config::default();
        let ov = crate::config::RoomOverride {
            unencrypted: Some(NotifyLevel::Noisy),
            ..Default::default()
        };
        config
            .notifications
            .rooms
            .overrides
            .insert("!room:example.com".into(), ov);
        // group room normally silent → override to noisy
        let c = EventContext { is_direct: false, ..ctx() };
        assert_eq!(
            evaluate(&c, &config, chrono::Local::now()),
            FilterResult::Notify { sound: true }
        );
    }
}
