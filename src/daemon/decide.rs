use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};

use crate::config::Config;
use crate::filters::{self, EventContext, EventKind, FilterResult};
use crate::notification::{Notification, Notifier};

use super::state::DaemonState;

/// the decision a handler arrives at after evaluating filters/state.
/// extracted to make the decision logic testable without matrix-sdk types.
#[derive(Debug, PartialEq)]
pub(super) enum NotifAction {
    Suppress,
    Send {
        tag: String,
        title: String,
        subtitle: Option<String>,
        body: String,
        sound: Option<String>,
        replace: bool,
        /// when Some, dispatch_action records this notification in DaemonState
        /// for later edit lookup, redaction dismissal, and receipt-based dismissal.
        /// invites, replace-mode edits, and noisy edits (which reuse an existing
        /// tracked entry's tag) use None.
        track: Option<Tracked>,
    },
}

/// pairs an event id with its origin_server_ts (seconds), used for
/// notification history tracking and precise receipt-based dismissal.
#[derive(Debug, PartialEq)]
pub(super) struct Tracked {
    pub event_id: OwnedEventId,
    pub event_ts: i64,
}

/// input for `decide_message_notif` describing an incoming room message or edit
pub(super) struct MessageNotif {
    pub kind: EventKind,
    pub sender: String,
    pub sender_name: String,
    pub room_id: OwnedRoomId,
    pub room_name: String,
    pub is_direct: bool,
    pub is_encrypted: bool,
    pub mentions_you: bool,
    pub mentions_room: bool,
    pub matched_keyword: bool,
    pub event_ts_secs: i64,
    pub event_id: OwnedEventId,
    pub edited_event_id: Option<OwnedEventId>,
    pub body: String,
}

/// input for `decide_invite_notif` describing a room invite.
/// invites are stripped state events without a canonical origin_server_ts;
/// the handler synthesizes one from the wall clock so the filter pipeline
/// (max_event_age check) treats them as fresh
pub(super) struct InviteNotif {
    pub sender: String,
    pub sender_localpart: String,
    pub room_id: OwnedRoomId,
    pub room_name: String,
    pub event_ts_secs: i64,
}

/// input for `decide_reaction_notif`. only fires for reactions to our own messages
pub(super) struct ReactionNotif {
    pub reactor_user_id: String,
    pub reactor_name: String,
    pub target_event_id: OwnedEventId,
    pub reaction_event_id: OwnedEventId,
    pub reaction_key: String,
    pub original_body: String,
    pub is_direct: bool,
    pub is_encrypted: bool,
    pub event_ts_secs: i64,
    pub room_id: OwnedRoomId,
}

/// input for `decide_call_notif` describing a legacy 1:1 incoming call
pub(super) struct CallNotif {
    pub caller_user_id: String,
    pub caller_name: String,
    pub call_id: String,
    pub call_event_id: OwnedEventId,
    pub is_direct: bool,
    pub is_encrypted: bool,
    pub event_ts_secs: i64,
    pub room_id: OwnedRoomId,
}

/// pure decision logic for messages and edits.
/// reads from DaemonState (lookup_tag, apply_debounce) but never calls record_notif;
/// the caller does that after a successful send.
pub(super) async fn decide_message_notif(
    n: MessageNotif,
    state: &DaemonState,
    config: &Config,
) -> NotifAction {
    let ctx = EventContext {
        kind: n.kind,
        sender: n.sender.clone(),
        room_id: n.room_id.to_string(),
        is_direct: n.is_direct,
        is_encrypted: n.is_encrypted,
        mentions_you: n.mentions_you,
        mentions_room: n.mentions_room,
        matched_keyword: n.matched_keyword,
        event_ts_secs: n.event_ts_secs,
    };

    let result = filters::evaluate(&ctx, config, chrono::Local::now());
    let effective = config.notifications.effective_for(n.room_id.as_str(), n.is_direct);

    let (title, subtitle) = if n.is_direct {
        (n.sender_name.clone(), None)
    } else {
        (n.room_name.clone(), Some(n.sender_name.clone()))
    };

    match result {
        FilterResult::Suppress => NotifAction::Suppress,
        FilterResult::Replace => {
            let Some(orig_id) = n.edited_event_id else { return NotifAction::Suppress };
            let Some(tag) = state.lookup_tag(&n.room_id, &orig_id).await else {
                return NotifAction::Suppress;
            };
            NotifAction::Send {
                tag,
                title,
                subtitle,
                body: n.body,
                sound: None,
                replace: true,
                track: None, // replace doesn't add a new history entry
            }
        }
        FilterResult::Notify { sound: play_sound } => {
            // for noisy edits, we reuse the original notification's tag so the
            // notifier replaces the visible content. since the original is
            // already tracked under that event_id, set track: None to avoid
            // adding a duplicate history entry per edit.
            let is_edit = matches!(n.kind, EventKind::Edit);
            let (tag, track, edit_title) = if is_edit {
                let Some(orig_id) = n.edited_event_id else { return NotifAction::Suppress };
                let Some(tag) = state.lookup_tag(&n.room_id, &orig_id).await else {
                    return NotifAction::Suppress;
                };
                (tag, None, Some(format!("{} edited their message", n.sender_name)))
            } else {
                let track = Tracked { event_id: n.event_id.clone(), event_ts: n.event_ts_secs };
                (format!("msg:{}", n.event_id), Some(track), None)
            };

            let final_sound = state
                .apply_debounce(
                    &n.room_id,
                    effective.noisy_debounce_seconds,
                    config.notifications.senders.contains(&n.sender),
                    play_sound,
                )
                .await;

            NotifAction::Send {
                tag,
                title: edit_title.unwrap_or(title),
                subtitle,
                body: n.body,
                sound: if final_sound { Some(config.notifications.sound.clone()) } else { None },
                replace: false,
                track,
            }
        }
    }
}

/// pure decision logic for invites. invites are not tracked in DaemonState since
/// they're state events without a canonical event id we'd ever need to look up
/// (no edits, no redactions in the typical sense, no per-event read receipts)
pub(super) async fn decide_invite_notif(n: InviteNotif, config: &Config) -> NotifAction {
    let ctx = EventContext {
        kind: EventKind::Invite,
        sender: n.sender.clone(),
        room_id: n.room_id.to_string(),
        is_direct: false,
        is_encrypted: false,
        mentions_you: false,
        mentions_room: false,
        matched_keyword: false,
        event_ts_secs: n.event_ts_secs,
    };
    match filters::evaluate(&ctx, config, chrono::Local::now()) {
        FilterResult::Notify { sound } => NotifAction::Send {
            tag: format!("invite:{}", n.room_id),
            title: format!("invite to {}", n.room_name),
            subtitle: Some(format!("from {}", n.sender_localpart)),
            body: String::new(),
            sound: if sound { Some(config.notifications.sound.clone()) } else { None },
            replace: false,
            track: None,
        },
        _ => NotifAction::Suppress,
    }
}

/// pure decision logic for reactions
pub(super) async fn decide_reaction_notif(
    n: ReactionNotif,
    state: &DaemonState,
    config: &Config,
) -> NotifAction {
    let ctx = EventContext {
        kind: EventKind::Reaction,
        sender: n.reactor_user_id.clone(),
        room_id: n.room_id.to_string(),
        is_direct: n.is_direct,
        is_encrypted: n.is_encrypted,
        mentions_you: false,
        mentions_room: false,
        matched_keyword: false,
        event_ts_secs: n.event_ts_secs,
    };

    let play_sound = match filters::evaluate(&ctx, config, chrono::Local::now()) {
        FilterResult::Notify { sound } => sound,
        _ => return NotifAction::Suppress,
    };

    let body = filters::truncate(&n.original_body, config.behavior.max_body_length);
    let effective = config.notifications.effective_for(&ctx.room_id, n.is_direct);

    let final_sound = state
        .apply_debounce(
            &n.room_id,
            effective.noisy_debounce_seconds,
            config.notifications.senders.contains(&n.reactor_user_id),
            play_sound,
        )
        .await;

    NotifAction::Send {
        tag: format!("react:{}", n.target_event_id),
        title: format!("{} reacted {}", n.reactor_name, n.reaction_key),
        subtitle: None,
        body,
        sound: if final_sound { Some(config.notifications.sound.clone()) } else { None },
        replace: false,
        track: Some(Tracked { event_id: n.reaction_event_id, event_ts: n.event_ts_secs }),
    }
}

/// pure decision logic for legacy 1:1 call invites
pub(super) async fn decide_call_notif(n: CallNotif, config: &Config) -> NotifAction {
    let ctx = EventContext {
        kind: EventKind::Call,
        sender: n.caller_user_id.clone(),
        room_id: n.room_id.to_string(),
        is_direct: n.is_direct,
        is_encrypted: n.is_encrypted,
        mentions_you: false,
        mentions_room: false,
        matched_keyword: false,
        event_ts_secs: n.event_ts_secs,
    };
    match filters::evaluate(&ctx, config, chrono::Local::now()) {
        FilterResult::Notify { sound } => NotifAction::Send {
            tag: format!("call:{}", n.call_id),
            title: format!("incoming call from {}", n.caller_name),
            subtitle: None,
            body: String::new(),
            sound: if sound { Some(config.notifications.sound.clone()) } else { None },
            replace: false,
            track: Some(Tracked { event_id: n.call_event_id, event_ts: n.event_ts_secs }),
        },
        _ => NotifAction::Suppress,
    }
}

/// dispatch a decided action to the notifier and update state.
/// the macOS notification thread_id is derived from room_id (one stack per room).
/// matrix m.thread relations could later derive a finer-grained thread_id,
/// but reactions/calls in threads need extra wiring so it's deferred for now.
pub(super) async fn dispatch_action(
    action: NotifAction,
    notifier: &dyn Notifier,
    state: &DaemonState,
    room_id: &OwnedRoomId,
) {
    match action {
        NotifAction::Suppress => {}
        NotifAction::Send { tag, title, subtitle, body, sound, replace, track } => {
            let notification = Notification {
                tag: tag.clone(),
                title,
                subtitle,
                body,
                sound,
                thread_id: room_id.to_string(),
                replace,
            };
            if let Err(e) = notifier.send(&notification) {
                tracing::error!(error = %e, "failed to send notification");
                return;
            }
            if let Some(Tracked { event_id, event_ts }) = track {
                state.record_notif(room_id, tag, event_id, event_ts).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EditMode, NotifyLevel, RoomOverride};
    use matrix_sdk::ruma::{room_id, user_id, OwnedUserId};
    use std::sync::atomic::Ordering;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn me() -> OwnedUserId {
        user_id!("@me:example.com").to_owned()
    }
    fn room1() -> OwnedRoomId {
        room_id!("!room1:example.com").to_owned()
    }
    fn evt(n: u32) -> OwnedEventId {
        OwnedEventId::try_from(format!("$evt{n}:example.com")).unwrap()
    }
    fn state() -> DaemonState {
        let s = DaemonState::new(me());
        s.initial_sync_done.store(true, Ordering::Relaxed);
        s
    }

    /// drop-in mock notifier that records all sends and dismisses
    struct MockNotifier {
        sent: StdMutex<Vec<Notification>>,
        dismissed: StdMutex<Vec<String>>,
    }
    impl MockNotifier {
        fn new() -> Self {
            Self {
                sent: StdMutex::new(Vec::new()),
                dismissed: StdMutex::new(Vec::new()),
            }
        }
        fn sent_tags(&self) -> Vec<String> {
            self.sent.lock().unwrap().iter().map(|n| n.tag.clone()).collect()
        }
    }
    impl Notifier for MockNotifier {
        fn send(&self, notification: &Notification) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(Notification {
                tag: notification.tag.clone(),
                title: notification.title.clone(),
                subtitle: notification.subtitle.clone(),
                body: notification.body.clone(),
                sound: notification.sound.clone(),
                thread_id: notification.thread_id.clone(),
                replace: notification.replace,
            });
            Ok(())
        }
        fn dismiss(&self, tag: &str) -> anyhow::Result<()> {
            self.dismissed.lock().unwrap().push(tag.to_string());
            Ok(())
        }
    }

    fn dms_default_config() -> Config {
        Config::default()
    }

    /// builder helper. tests override only the fields they care about.
    fn msg(event_id: OwnedEventId) -> MessageNotif {
        MessageNotif {
            kind: EventKind::Message,
            sender: "@alice:x".into(),
            sender_name: "alice".into(),
            room_id: room1(),
            room_name: "alice".into(),
            is_direct: true,
            is_encrypted: false,
                mentions_you: false,
            mentions_room: false,
            matched_keyword: false,
            event_ts_secs: chrono::Utc::now().timestamp(),
            event_id,
            edited_event_id: None,
            body: "hi".into(),
        }
    }

    #[tokio::test]
    async fn decide_message_dm_default_sends_with_msg_tag() {
        let s = state();
        let c = dms_default_config();
        let action = decide_message_notif(msg(evt(1)), &s, &c).await;
        match action {
            NotifAction::Send { tag, sound, replace, .. } => {
                assert_eq!(tag, format!("msg:{}", evt(1)));
                assert!(sound.is_some());
                assert!(!replace);
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_message_room_silent_default() {
        let s = state();
        let c = dms_default_config();
        let action = decide_message_notif(
            MessageNotif { is_direct: false, room_name: "the room".into(), ..msg(evt(1)) },
            &s,
            &c,
        )
        .await;
        match action {
            NotifAction::Send { sound, .. } => assert!(sound.is_none()),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_message_with_mention_upgrades_to_noisy() {
        let s = state();
        let c = dms_default_config();
        let action = decide_message_notif(
            MessageNotif {
                is_direct: false,
                mentions_you: true,
                room_name: "the room".into(),
                ..msg(evt(1))
            },
            &s,
            &c,
        )
        .await;
        match action {
            NotifAction::Send { sound, .. } => assert!(sound.is_some()),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_edit_replace_when_original_tracked() {
        let s = state();
        let c = dms_default_config();
        s.record_notif(&room1(), "msg:original".into(), evt(1), 0).await;

        let action = decide_message_notif(
            MessageNotif {
                kind: EventKind::Edit,
                edited_event_id: Some(evt(1)),
                body: "edited body".into(),
                ..msg(evt(2))
            },
            &s,
            &c,
        )
        .await;
        match action {
            NotifAction::Send { tag, sound, replace, .. } => {
                assert_eq!(tag, "msg:original");
                assert!(sound.is_none());
                assert!(replace);
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_edit_replace_when_original_not_tracked_suppresses() {
        let s = state();
        let c = dms_default_config();
        let action = decide_message_notif(
            MessageNotif {
                kind: EventKind::Edit,
                edited_event_id: Some(evt(1)),
                ..msg(evt(2))
            },
            &s,
            &c,
        )
        .await;
        assert_eq!(action, NotifAction::Suppress);
    }

    #[tokio::test]
    async fn decide_edit_silent_when_original_not_tracked_suppresses() {
        let s = state();
        let mut c = dms_default_config();
        c.notifications.dms.edits = EditMode::Silent;
        let action = decide_message_notif(
            MessageNotif {
                kind: EventKind::Edit,
                edited_event_id: Some(evt(1)),
                ..msg(evt(2))
            },
            &s,
            &c,
        )
        .await;
        assert_eq!(action, NotifAction::Suppress);
    }

    #[tokio::test]
    async fn decide_edit_noisy_when_original_tracked_with_edit_title() {
        let s = state();
        let mut c = dms_default_config();
        c.notifications.dms.edits = EditMode::Noisy;
        s.record_notif(&room1(), "msg:original".into(), evt(1), 0).await;

        let action = decide_message_notif(
            MessageNotif {
                kind: EventKind::Edit,
                edited_event_id: Some(evt(1)),
                body: "edited body".into(),
                ..msg(evt(2))
            },
            &s,
            &c,
        )
        .await;
        match action {
            NotifAction::Send { tag, title, sound, replace, .. } => {
                assert_eq!(tag, "msg:original");
                assert_eq!(title, "alice edited their message");
                assert!(sound.is_some());
                assert!(!replace);
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_edit_noisy_does_not_duplicate_tracking_entry() {
        // regression: noisy edits used to set event_id=Some(orig_id), which made
        // dispatch_action call record_notif each time, growing duplicate entries.
        let s = state();
        let mut c = dms_default_config();
        c.notifications.dms.edits = EditMode::Noisy;
        let notifier = MockNotifier::new();
        s.record_notif(&room1(), "msg:original".into(), evt(1), 0).await;

        for i in 2..=4 {
            let action = decide_message_notif(
                MessageNotif {
                    kind: EventKind::Edit,
                    edited_event_id: Some(evt(1)),
                    body: format!("edit {i}"),
                    ..msg(evt(i))
                },
                &s,
                &c,
            )
            .await;
            dispatch_action(action, &notifier, &s, &room1()).await;
        }

        let remaining = s.drain_room_tags(&room1()).await;
        assert_eq!(remaining.len(), 1, "expected exactly one entry after 3 edits, got {}", remaining.len());
        assert_eq!(remaining[0], "msg:original");
    }

    #[tokio::test(start_paused = true)]
    async fn decide_message_debounced_within_window_silent() {
        let s = state();
        let mut c = dms_default_config();
        c.notifications.dms.noisy_debounce_seconds = 5;

        let _ = decide_message_notif(msg(evt(1)), &s, &c).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let action = decide_message_notif(msg(evt(2)), &s, &c).await;

        match action {
            NotifAction::Send { sound, .. } => assert!(sound.is_none(), "expected debounced silent"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn decide_message_debounce_bypassed_for_explicit_sender() {
        let s = state();
        let mut c = dms_default_config();
        c.notifications.dms.noisy_debounce_seconds = 5;
        c.notifications.senders.noisy.push("@alice:x".into());

        let _ = decide_message_notif(msg(evt(1)), &s, &c).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let action = decide_message_notif(msg(evt(2)), &s, &c).await;

        match action {
            NotifAction::Send { sound, .. } => assert!(sound.is_some(), "explicit sender bypasses debounce"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_invite_uses_invite_tag() {
        let c = dms_default_config();
        let action = decide_invite_notif(
            InviteNotif {
                sender: "@alice:x".into(),
                sender_localpart: "alice".into(),
                room_id: room1(),
                room_name: "test room".into(),
                event_ts_secs: chrono::Utc::now().timestamp(),
            },
            &c,
        )
        .await;
        match action {
            NotifAction::Send { tag, sound, .. } => {
                assert_eq!(tag, format!("invite:{}", room1()));
                assert!(sound.is_some());
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_reaction_uses_react_tag() {
        let s = state();
        let c = dms_default_config();
        let action = decide_reaction_notif(
            ReactionNotif {
                reactor_user_id: "@bob:x".into(),
                reactor_name: "bob".into(),
                target_event_id: evt(1),
                reaction_event_id: evt(2),
                reaction_key: "👍".into(),
                original_body: "great message".into(),
                is_direct: true,
                is_encrypted: false,
                event_ts_secs: chrono::Utc::now().timestamp(),
                room_id: room1(),
            },
            &s,
            &c,
        )
        .await;
        match action {
            NotifAction::Send { tag, title, body, .. } => {
                assert_eq!(tag, format!("react:{}", evt(1)));
                assert!(title.contains("bob"));
                assert!(title.contains("👍"));
                assert!(body.contains("great message"));
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_call_uses_call_tag() {
        let c = dms_default_config();
        let action = decide_call_notif(
            CallNotif {
                caller_user_id: "@bob:x".into(),
                caller_name: "bob".into(),
                call_id: "callid123".into(),
                call_event_id: evt(1),
                is_direct: true,
                is_encrypted: false,
                event_ts_secs: chrono::Utc::now().timestamp(),
                room_id: room1(),
            },
            &c,
        )
        .await;
        match action {
            NotifAction::Send { tag, title, .. } => {
                assert_eq!(tag, "call:callid123");
                assert!(title.contains("bob"));
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_action_records_in_history_and_skips_for_replace() {
        let s = state();
        let notifier = MockNotifier::new();

        let send_action = NotifAction::Send {
            tag: "msg:abc".into(),
            title: "t".into(),
            subtitle: None,
            body: "b".into(),
            sound: None,
            replace: false,
            track: Some(Tracked { event_id: evt(1), event_ts: 100 }),
        };
        dispatch_action(send_action, &notifier, &s, &room1()).await;
        assert_eq!(s.lookup_tag(&room1(), &evt(1)).await, Some("msg:abc".into()));

        // replace action should NOT record
        let replace_action = NotifAction::Send {
            tag: "msg:abc".into(),
            title: "t".into(),
            subtitle: None,
            body: "edited".into(),
            sound: None,
            replace: true,
            track: None,
        };
        dispatch_action(replace_action, &notifier, &s, &room1()).await;
        assert_eq!(s.lookup_tag(&room1(), &evt(99)).await, None);

        // both notifications should have been sent
        assert_eq!(notifier.sent_tags(), vec!["msg:abc".to_string(), "msg:abc".to_string()]);
    }

    #[tokio::test]
    async fn decide_message_respects_room_override() {
        let s = state();
        let mut c = dms_default_config();
        let ov = RoomOverride {
            unencrypted: Some(NotifyLevel::Noisy),
            ..Default::default()
        };
        c.notifications.rooms.overrides.insert(room1().to_string(), ov);

        let action = decide_message_notif(
            MessageNotif { is_direct: false, room_name: "the room".into(), ..msg(evt(1)) },
            &s,
            &c,
        )
        .await;
        match action {
            NotifAction::Send { sound, .. } => assert!(sound.is_some()),
            other => panic!("expected Send, got {other:?}"),
        }
    }
}
