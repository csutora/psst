use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, OwnedUserId};
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;

pub(super) const NOTIF_HISTORY_CAP: usize = 100;
pub(super) const NOTIF_HISTORY_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(super) const NOTIF_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const SEEN_EVENTS_CAP: usize = 10_000;

/// per-room outstanding notification tracking
#[derive(Clone, Debug)]
pub(super) struct NotifEntry {
    sent_at: Instant,
    tag: String,
    event_id: OwnedEventId,
    /// the event's origin_server_ts in seconds. used to dismiss precisely on read receipts
    event_ts: i64,
}

type NotifHistory = HashMap<OwnedRoomId, VecDeque<NotifEntry>>;

pub(super) struct DaemonState {
    pub(super) own_user_id: OwnedUserId,
    pub(super) display_name: RwLock<Option<String>>,
    pub(super) initial_sync_done: AtomicBool,
    seen_events: Mutex<HashSet<OwnedEventId>>,
    last_notif_per_room: Mutex<HashMap<OwnedRoomId, Instant>>,
    notifications_by_room: Mutex<NotifHistory>,
}

impl DaemonState {
    pub(super) fn new(own_user_id: OwnedUserId) -> Self {
        Self {
            own_user_id,
            display_name: RwLock::new(None),
            initial_sync_done: AtomicBool::new(false),
            seen_events: Mutex::new(HashSet::new()),
            last_notif_per_room: Mutex::new(HashMap::new()),
            notifications_by_room: Mutex::new(HashMap::new()),
        }
    }

    /// returns true if this is the first time we see the event
    pub(super) async fn dedup(&self, event_id: &OwnedEventId) -> bool {
        let mut seen = self.seen_events.lock().await;
        let inserted = seen.insert(event_id.to_owned());
        if seen.len() > SEEN_EVENTS_CAP {
            seen.clear();
        }
        inserted
    }

    /// record a notification for a room. drops oldest if over cap.
    pub(super) async fn record_notif(
        &self,
        room_id: &OwnedRoomId,
        tag: String,
        event_id: OwnedEventId,
        event_ts: i64,
    ) {
        let mut by_room = self.notifications_by_room.lock().await;
        let entries = by_room.entry(room_id.clone()).or_default();
        let now = Instant::now();
        while entries.front().is_some_and(|e| now.duration_since(e.sent_at) > NOTIF_HISTORY_TTL) {
            entries.pop_front();
        }
        entries.push_back(NotifEntry { sent_at: now, tag, event_id, event_ts });
        if entries.len() > NOTIF_HISTORY_CAP {
            entries.pop_front();
        }
    }

    /// look up the tag we used for a given event in a room, if any
    pub(super) async fn lookup_tag(
        &self,
        room_id: &OwnedRoomId,
        event_id: &OwnedEventId,
    ) -> Option<String> {
        let by_room = self.notifications_by_room.lock().await;
        by_room
            .get(room_id)
            .and_then(|entries| entries.iter().find(|e| &e.event_id == event_id))
            .map(|e| e.tag.clone())
    }

    /// take and remove the entry matching the given event_id, returning its tag if found
    pub(super) async fn remove_by_event_id(
        &self,
        room_id: &OwnedRoomId,
        event_id: &OwnedEventId,
    ) -> Option<String> {
        let mut by_room = self.notifications_by_room.lock().await;
        let entries = by_room.get_mut(room_id)?;
        let pos = entries.iter().position(|e| &e.event_id == event_id)?;
        let removed = entries.remove(pos);
        if entries.is_empty() {
            by_room.remove(room_id);
        }
        removed.map(|e| e.tag)
    }

    /// take and remove the entry matching the given tag, returning whether found
    pub(super) async fn remove_by_tag(&self, room_id: &OwnedRoomId, tag: &str) -> bool {
        let mut by_room = self.notifications_by_room.lock().await;
        let Some(entries) = by_room.get_mut(room_id) else { return false };
        let Some(pos) = entries.iter().position(|e| e.tag == tag) else { return false };
        entries.remove(pos);
        if entries.is_empty() {
            by_room.remove(room_id);
        }
        true
    }

    /// take all notification tags for a room and clear the room's history.
    /// production fallback for receipts that arrive without a `ts` field
    pub(super) async fn drain_room_tags(&self, room_id: &OwnedRoomId) -> Vec<String> {
        let mut by_room = self.notifications_by_room.lock().await;
        by_room
            .remove(room_id)
            .map(|entries| entries.into_iter().map(|e| e.tag).collect())
            .unwrap_or_default()
    }

    /// drain entries with `event_ts <= cutoff_ts`, returning their tags.
    /// used for read receipt dismissal: dismiss everything seen up to the receipt's timestamp.
    pub(super) async fn drain_room_tags_up_to(
        &self,
        room_id: &OwnedRoomId,
        cutoff_ts: i64,
    ) -> Vec<String> {
        let mut by_room = self.notifications_by_room.lock().await;
        let Some(entries) = by_room.get_mut(room_id) else { return Vec::new() };
        let mut drained = Vec::new();
        entries.retain(|e| {
            if e.event_ts <= cutoff_ts {
                drained.push(e.tag.clone());
                false
            } else {
                true
            }
        });
        if entries.is_empty() {
            by_room.remove(room_id);
        }
        drained
    }

    /// drop notification entries older than NOTIF_HISTORY_TTL across all rooms,
    /// and remove rooms whose deque is empty after pruning
    pub(super) async fn cleanup_expired_notifs(&self) {
        let mut by_room = self.notifications_by_room.lock().await;
        let now = Instant::now();
        for entries in by_room.values_mut() {
            while entries.front().is_some_and(|e| now.duration_since(e.sent_at) > NOTIF_HISTORY_TTL) {
                entries.pop_front();
            }
        }
        by_room.retain(|_, entries| !entries.is_empty());
    }

    /// resolve final sound state honoring per-room debounce, then mark this room
    /// as having sent a notification (resetting the debounce window).
    /// `bypass` lets explicit-noisy senders skip the debounce check.
    pub(super) async fn apply_debounce(
        &self,
        room_id: &OwnedRoomId,
        debounce_seconds: u64,
        bypass: bool,
        play_sound: bool,
    ) -> bool {
        let final_sound = if play_sound
            && !bypass
            && self.is_within_debounce(room_id, debounce_seconds).await
        {
            false
        } else {
            play_sound
        };
        self.mark_notification(room_id).await;
        final_sound
    }

    /// read-only check: would a noisy notification right now be within the debounce window?
    pub(super) async fn is_within_debounce(
        &self,
        room_id: &OwnedRoomId,
        debounce_seconds: u64,
    ) -> bool {
        if debounce_seconds == 0 {
            return false;
        }
        let last = self.last_notif_per_room.lock().await;
        last.get(room_id).is_some_and(|prev| {
            Instant::now().duration_since(*prev) < Duration::from_secs(debounce_seconds)
        })
    }

    /// stamp "we just sent a notification for this room" - resets the debounce window
    pub(super) async fn mark_notification(&self, room_id: &OwnedRoomId) {
        self.last_notif_per_room
            .lock()
            .await
            .insert(room_id.clone(), Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{room_id, user_id};
    use std::sync::atomic::Ordering;

    fn me() -> OwnedUserId {
        user_id!("@me:example.com").to_owned()
    }
    fn room1() -> OwnedRoomId {
        room_id!("!room1:example.com").to_owned()
    }
    fn room2() -> OwnedRoomId {
        room_id!("!room2:example.com").to_owned()
    }
    fn evt(n: u32) -> OwnedEventId {
        OwnedEventId::try_from(format!("$evt{n}:example.com")).unwrap()
    }
    fn state() -> DaemonState {
        let s = DaemonState::new(me());
        s.initial_sync_done.store(true, Ordering::Relaxed);
        s
    }

    #[tokio::test]
    async fn dedup_first_seen_returns_true() {
        let s = state();
        assert!(s.dedup(&evt(1)).await);
    }

    #[tokio::test]
    async fn dedup_repeat_returns_false() {
        let s = state();
        assert!(s.dedup(&evt(1)).await);
        assert!(!s.dedup(&evt(1)).await);
    }

    #[tokio::test]
    async fn record_notif_within_cap_keeps_all() {
        let s = state();
        for i in 0..50 {
            s.record_notif(&room1(), format!("tag:{i}"), evt(i), 0).await;
        }
        let tags = s.drain_room_tags(&room1()).await;
        assert_eq!(tags.len(), 50);
    }

    #[tokio::test]
    async fn record_notif_at_cap_evicts_oldest() {
        let s = state();
        for i in 0..(NOTIF_HISTORY_CAP + 5) as u32 {
            s.record_notif(&room1(), format!("tag:{i}"), evt(i), 0).await;
        }
        let tags = s.drain_room_tags(&room1()).await;
        assert_eq!(tags.len(), NOTIF_HISTORY_CAP);
        assert_eq!(tags.first().unwrap(), "tag:5");
        assert_eq!(tags.last().unwrap(), &format!("tag:{}", NOTIF_HISTORY_CAP + 4));
    }

    #[tokio::test(start_paused = true)]
    async fn record_notif_expires_old_on_insert() {
        let s = state();
        s.record_notif(&room1(), "old".into(), evt(1), 0).await;
        tokio::time::advance(NOTIF_HISTORY_TTL + Duration::from_secs(1)).await;
        s.record_notif(&room1(), "new".into(), evt(2), 0).await;
        let tags = s.drain_room_tags(&room1()).await;
        assert_eq!(tags, vec!["new".to_string()]);
    }

    #[tokio::test]
    async fn lookup_tag_finds_existing() {
        let s = state();
        s.record_notif(&room1(), "msg:abc".into(), evt(1), 0).await;
        let found = s.lookup_tag(&room1(), &evt(1)).await;
        assert_eq!(found, Some("msg:abc".to_string()));
    }

    #[tokio::test]
    async fn lookup_tag_returns_none_when_event_absent() {
        let s = state();
        s.record_notif(&room1(), "msg:abc".into(), evt(1), 0).await;
        let found = s.lookup_tag(&room1(), &evt(2)).await;
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn lookup_tag_returns_none_when_room_unknown() {
        let s = state();
        s.record_notif(&room1(), "msg:abc".into(), evt(1), 0).await;
        let found = s.lookup_tag(&room2(), &evt(1)).await;
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn drain_room_tags_returns_all_and_clears() {
        let s = state();
        s.record_notif(&room1(), "a".into(), evt(1), 0).await;
        s.record_notif(&room1(), "b".into(), evt(2), 0).await;
        let tags = s.drain_room_tags(&room1()).await;
        assert_eq!(tags, vec!["a".to_string(), "b".to_string()]);
        let tags2 = s.drain_room_tags(&room1()).await;
        assert!(tags2.is_empty());
    }

    #[tokio::test]
    async fn drain_room_tags_returns_empty_for_unknown_room() {
        let s = state();
        let tags = s.drain_room_tags(&room1()).await;
        assert!(tags.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_expired_notifs_drops_old_keeps_fresh() {
        let s = state();
        s.record_notif(&room1(), "old".into(), evt(1), 0).await;
        tokio::time::advance(NOTIF_HISTORY_TTL + Duration::from_secs(1)).await;
        s.record_notif(&room1(), "fresh".into(), evt(2), 0).await;
        s.cleanup_expired_notifs().await;
        let tags = s.drain_room_tags(&room1()).await;
        assert_eq!(tags, vec!["fresh".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_removes_empty_rooms() {
        let s = state();
        s.record_notif(&room1(), "old".into(), evt(1), 0).await;
        tokio::time::advance(NOTIF_HISTORY_TTL + Duration::from_secs(1)).await;
        s.cleanup_expired_notifs().await;
        let by_room = s.notifications_by_room.lock().await;
        assert!(!by_room.contains_key(&room1()));
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_first_call_returns_false() {
        let s = state();
        assert!(!s.is_within_debounce(&room1(), 5).await);
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_within_window_returns_true() {
        let s = state();
        s.mark_notification(&room1()).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(s.is_within_debounce(&room1(), 5).await);
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_after_window_returns_false() {
        let s = state();
        s.mark_notification(&room1()).await;
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(!s.is_within_debounce(&room1(), 5).await);
    }

    #[tokio::test]
    async fn debounce_zero_seconds_disables() {
        let s = state();
        s.mark_notification(&room1()).await;
        assert!(!s.is_within_debounce(&room1(), 0).await);
    }

    #[tokio::test(start_paused = true)]
    async fn apply_debounce_silences_within_window_when_not_bypassed() {
        let s = state();
        let _ = s.apply_debounce(&room1(), 5, false, true).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let final_sound = s.apply_debounce(&room1(), 5, false, true).await;
        assert!(!final_sound);
    }

    #[tokio::test(start_paused = true)]
    async fn apply_debounce_keeps_sound_when_bypassed() {
        let s = state();
        let _ = s.apply_debounce(&room1(), 5, false, true).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let final_sound = s.apply_debounce(&room1(), 5, true, true).await;
        assert!(final_sound);
    }

    #[tokio::test]
    async fn drain_room_tags_up_to_dismisses_only_old() {
        let s = state();
        s.record_notif(&room1(), "old:1".into(), evt(1), 100).await;
        s.record_notif(&room1(), "old:2".into(), evt(2), 200).await;
        s.record_notif(&room1(), "fresh:3".into(), evt(3), 400).await;

        let dismissed = s.drain_room_tags_up_to(&room1(), 250).await;
        assert_eq!(dismissed, vec!["old:1".to_string(), "old:2".to_string()]);

        let remaining = s.drain_room_tags(&room1()).await;
        assert_eq!(remaining, vec!["fresh:3".to_string()]);
    }

    #[tokio::test]
    async fn drain_room_tags_up_to_returns_empty_for_unknown_room() {
        let s = state();
        let dismissed = s.drain_room_tags_up_to(&room1(), 1_000_000).await;
        assert!(dismissed.is_empty());
    }

    #[tokio::test]
    async fn remove_by_event_id_finds_and_removes() {
        let s = state();
        s.record_notif(&room1(), "msg:abc".into(), evt(1), 0).await;
        let tag = s.remove_by_event_id(&room1(), &evt(1)).await;
        assert_eq!(tag, Some("msg:abc".to_string()));
        assert_eq!(s.lookup_tag(&room1(), &evt(1)).await, None);
    }

    #[tokio::test]
    async fn remove_by_event_id_returns_none_when_unknown() {
        let s = state();
        s.record_notif(&room1(), "msg:abc".into(), evt(1), 0).await;
        let tag = s.remove_by_event_id(&room1(), &evt(99)).await;
        assert_eq!(tag, None);
    }

    #[tokio::test]
    async fn remove_by_tag_finds_and_removes() {
        let s = state();
        s.record_notif(&room1(), "call:abc".into(), evt(1), 0).await;
        assert!(s.remove_by_tag(&room1(), "call:abc").await);
        assert_eq!(s.lookup_tag(&room1(), &evt(1)).await, None);
    }

    #[tokio::test]
    async fn remove_by_tag_returns_false_when_unknown() {
        let s = state();
        s.record_notif(&room1(), "call:abc".into(), evt(1), 0).await;
        assert!(!s.remove_by_tag(&room1(), "call:xyz").await);
    }

    #[tokio::test]
    async fn drain_after_record_clears_history() {
        let s = state();
        for i in 1..=5 {
            s.record_notif(&room1(), format!("tag:{i}"), evt(i), 0).await;
        }
        let drained = s.drain_room_tags(&room1()).await;
        assert_eq!(drained.len(), 5);
        assert_eq!(s.drain_room_tags(&room1()).await.len(), 0);
    }
}
