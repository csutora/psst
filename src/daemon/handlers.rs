use std::sync::atomic::Ordering;

use matrix_sdk::ruma::events::call::hangup::SyncCallHangupEvent;
use matrix_sdk::ruma::events::call::invite::SyncCallInviteEvent;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::receipt::{ReceiptEventContent, ReceiptType};
use matrix_sdk::ruma::events::room::member::{MembershipState, StrippedRoomMemberEvent};
use matrix_sdk::ruma::events::room::message::{Relation, SyncRoomMessageEvent};
use matrix_sdk::ruma::events::room::redaction::SyncRoomRedactionEvent;
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncEphemeralRoomEvent, SyncMessageLikeEvent,
};
use matrix_sdk::ruma::push::Action;
use matrix_sdk::Room;

use crate::config::Config;
use crate::filters::{self, EventKind};
use crate::notification::Notifier;

use super::decide::{
    decide_call_notif, decide_invite_notif, decide_message_notif, decide_reaction_notif,
    dispatch_action, CallNotif, InviteNotif, MessageNotif, ReactionNotif,
};
use super::state::DaemonState;

pub(super) async fn handle_message(
    ev: SyncRoomMessageEvent,
    room: Room,
    actions: Vec<Action>,
    state: &DaemonState,
    notifier: &dyn Notifier,
    config: &Config,
) {
    if ev.sender() == state.own_user_id {
        return;
    }
    if !state.initial_sync_done.load(Ordering::Relaxed) {
        return;
    }
    if !state.dedup(&ev.event_id().to_owned()).await {
        return;
    }

    let original = match ev.as_original() {
        Some(o) => o,
        None => return,
    };

    // server-side push rule decision; not used to gate notifications (psst trusts
    // its own filter pipeline + per-room overrides), kept only for log diagnostics
    let push_notify = actions.iter().any(|a| matches!(a, Action::Notify));

    let (kind, edited_event_id) = match original.content.relates_to.as_ref() {
        Some(Relation::Replacement(r)) => (EventKind::Edit, Some(r.event_id.clone())),
        _ => (EventKind::Message, None),
    };

    let is_direct = room.is_direct().await.unwrap_or(false);
    let is_encrypted = room.encryption_state().is_encrypted();

    let event_ts_millis: u64 = original.origin_server_ts.0.into();
    let event_ts_secs = (event_ts_millis / 1000) as i64;

    let sender_name = match room.get_member(ev.sender()).await {
        Ok(Some(member)) => member
            .display_name()
            .unwrap_or_else(|| ev.sender().localpart())
            .to_string(),
        _ => ev.sender().localpart().to_string(),
    };

    let body = filters::format_body(&original.content.msgtype, &sender_name, config);

    let effective = config
        .notifications
        .effective_for(room.room_id().as_str(), is_direct);
    let display_name = state.display_name.read().await.clone();
    let mentions = original
        .content
        .mentions
        .as_ref()
        .map(|m| (&m.user_ids, m.room));
    let (mentions_you, mentions_room, matched_keyword) = filters::detect_highlights(
        &body,
        mentions,
        &state.own_user_id,
        display_name.as_deref(),
        &effective.keywords,
    );

    let room_name = room
        .display_name()
        .await
        .map(|n| n.to_string())
        .unwrap_or_else(|_| room.room_id().to_string());

    tracing::info!(
        room_id = %room.room_id(),
        sender = %ev.sender(),
        ?kind,
        is_direct,
        is_encrypted,
        push_notify,
        mentions_you,
        mentions_room,
        matched_keyword,
        "received message"
    );

    let action = decide_message_notif(
        MessageNotif {
            kind,
            sender: ev.sender().to_string(),
            sender_name,
            room_id: room.room_id().to_owned(),
            room_name,
            is_direct,
            is_encrypted,
            mentions_you,
            mentions_room,
            matched_keyword,
            event_ts_secs,
            event_id: ev.event_id().to_owned(),
            edited_event_id,
            body,
        },
        state,
        config,
    )
    .await;

    dispatch_action(action, notifier, state, &room.room_id().to_owned()).await;
}

pub(super) async fn handle_invite(
    ev: StrippedRoomMemberEvent,
    room: Room,
    state: &DaemonState,
    notifier: &dyn Notifier,
    config: &Config,
) {
    if !state.initial_sync_done.load(Ordering::Relaxed) {
        return;
    }
    if ev.state_key != state.own_user_id {
        return;
    }
    if !matches!(ev.content.membership, MembershipState::Invite) {
        return;
    }

    let room_name = room
        .display_name()
        .await
        .map(|n| n.to_string())
        .unwrap_or_else(|_| room.room_id().to_string());

    let action = decide_invite_notif(
        InviteNotif {
            sender: ev.sender.to_string(),
            sender_localpart: ev.sender.localpart().to_string(),
            room_id: room.room_id().to_owned(),
            room_name,
            // stripped state events lack origin_server_ts; synthesize from wall
            // clock so the max_event_age check treats the invite as fresh
            event_ts_secs: chrono::Utc::now().timestamp(),
        },
        config,
    )
    .await;

    dispatch_action(action, notifier, state, &room.room_id().to_owned()).await;
}

pub(super) async fn handle_reaction(
    ev: SyncMessageLikeEvent<ReactionEventContent>,
    room: Room,
    state: &DaemonState,
    notifier: &dyn Notifier,
    config: &Config,
) {
    if !state.initial_sync_done.load(Ordering::Relaxed) {
        return;
    }

    let original_ev = match &ev {
        SyncMessageLikeEvent::Original(o) => o,
        _ => return,
    };

    if original_ev.sender == state.own_user_id {
        return;
    }
    if !state.dedup(&original_ev.event_id.to_owned()).await {
        return;
    }

    let target_event_id = original_ev.content.relates_to.event_id.clone();
    let reaction_key = original_ev.content.relates_to.key.clone();

    // fetch the original message and check it was sent by us.
    // typed deserialization handles encrypted/redacted shells correctly
    // (they fall through the variant match) and lets us reuse format_body
    // for emote/media/file framing
    let original_msg = match room.event(&target_event_id, None).await {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(error = %e, "could not fetch original message for reaction");
            return;
        }
    };

    let parsed: AnySyncTimelineEvent = match original_msg.raw().deserialize() {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(error = %e, "could not deserialize original message for reaction");
            return;
        }
    };
    let original = match parsed {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(o),
        )) => o,
        _ => return,
    };
    if original.sender != state.own_user_id {
        return;
    }

    // reuse the same body formatting messages use, so reaction-notification body
    // matches what the original message notification looked like
    let original_body = filters::format_body(
        &original.content.msgtype,
        original.sender.localpart(),
        config,
    );

    let is_direct = room.is_direct().await.unwrap_or(false);
    let is_encrypted = room.encryption_state().is_encrypted();
    let event_ts_millis: u64 = original_ev.origin_server_ts.0.into();
    let event_ts_secs = (event_ts_millis / 1000) as i64;

    let reactor_name = match room.get_member(&original_ev.sender).await {
        Ok(Some(member)) => member
            .display_name()
            .unwrap_or_else(|| original_ev.sender.localpart())
            .to_string(),
        _ => original_ev.sender.localpart().to_string(),
    };

    let action = decide_reaction_notif(
        ReactionNotif {
            reactor_user_id: original_ev.sender.to_string(),
            reactor_name,
            target_event_id,
            reaction_event_id: original_ev.event_id.to_owned(),
            reaction_key,
            original_body,
            is_direct,
            is_encrypted,
            event_ts_secs,
            room_id: room.room_id().to_owned(),
        },
        state,
        config,
    )
    .await;

    dispatch_action(action, notifier, state, &room.room_id().to_owned()).await;
}

pub(super) async fn handle_call_invite(
    ev: SyncCallInviteEvent,
    room: Room,
    state: &DaemonState,
    notifier: &dyn Notifier,
    config: &Config,
) {
    if !state.initial_sync_done.load(Ordering::Relaxed) {
        return;
    }

    let original = match &ev {
        SyncMessageLikeEvent::Original(o) => o,
        _ => return,
    };

    if original.sender == state.own_user_id {
        return;
    }
    if !state.dedup(&original.event_id.to_owned()).await {
        return;
    }
    if let Some(invitee) = &original.content.invitee {
        if invitee != &state.own_user_id {
            return;
        }
    }

    let is_direct = room.is_direct().await.unwrap_or(false);
    let is_encrypted = room.encryption_state().is_encrypted();
    let event_ts_millis: u64 = original.origin_server_ts.0.into();
    let event_ts_secs = (event_ts_millis / 1000) as i64;

    let caller_name = match room.get_member(&original.sender).await {
        Ok(Some(member)) => member
            .display_name()
            .unwrap_or_else(|| original.sender.localpart())
            .to_string(),
        _ => original.sender.localpart().to_string(),
    };

    let action = decide_call_notif(
        CallNotif {
            caller_user_id: original.sender.to_string(),
            caller_name,
            call_id: original.content.call_id.to_string(),
            call_event_id: original.event_id.to_owned(),
            is_direct,
            is_encrypted,
            event_ts_secs,
            room_id: room.room_id().to_owned(),
        },
        config,
    )
    .await;

    dispatch_action(action, notifier, state, &room.room_id().to_owned()).await;
}

pub(super) async fn handle_receipt(
    ev: SyncEphemeralRoomEvent<ReceiptEventContent>,
    room: Room,
    state: &DaemonState,
    notifier: &dyn Notifier,
) {
    if !state.initial_sync_done.load(Ordering::Relaxed) {
        return;
    }

    // find our own latest read receipt and its timestamp.
    // dismiss every notification we sent for events at or before that timestamp.
    let receipt = ev
        .content
        .user_receipt(&state.own_user_id, ReceiptType::Read)
        .or_else(|| ev.content.user_receipt(&state.own_user_id, ReceiptType::ReadPrivate));
    let Some((_event_id, receipt)) = receipt else { return };

    let room_id = room.room_id().to_owned();
    // prefer precise dismissal by timestamp; fall back to dismissing the entire room
    // if the receipt has no ts (matrix spec says SHOULD have it; some clients omit it)
    let tags = match receipt.ts {
        Some(ts) => {
            let cutoff_ts = i64::from(ts.get()) / 1000;
            state.drain_room_tags_up_to(&room_id, cutoff_ts).await
        }
        None => state.drain_room_tags(&room_id).await,
    };
    if tags.is_empty() {
        return;
    }
    tracing::debug!(room_id = %room_id, count = tags.len(), "dismissing on receipt");
    for tag in tags {
        if let Err(e) = notifier.dismiss(&tag) {
            tracing::error!(error = %e, "failed to dismiss notification");
        }
    }
}

pub(super) async fn handle_call_hangup(
    ev: SyncCallHangupEvent,
    room: Room,
    state: &DaemonState,
    notifier: &dyn Notifier,
) {
    if !state.initial_sync_done.load(Ordering::Relaxed) {
        return;
    }
    let SyncMessageLikeEvent::Original(original) = &ev else { return };
    let tag = format!("call:{}", original.content.call_id);
    let room_id = room.room_id().to_owned();
    if state.remove_by_tag(&room_id, &tag).await {
        if let Err(e) = notifier.dismiss(&tag) {
            tracing::error!(error = %e, "failed to dismiss call notification");
        }
    }
}

pub(super) async fn handle_redaction(
    ev: SyncRoomRedactionEvent,
    room: Room,
    state: &DaemonState,
    notifier: &dyn Notifier,
) {
    if !state.initial_sync_done.load(Ordering::Relaxed) {
        return;
    }
    let SyncRoomRedactionEvent::Original(original) = &ev else { return };
    let Some(redacted_id) = original.content.redacts.as_ref().or(original.redacts.as_ref()) else {
        return;
    };
    let room_id = room.room_id().to_owned();
    if let Some(tag) = state.remove_by_event_id(&room_id, redacted_id).await {
        if let Err(e) = notifier.dismiss(&tag) {
            tracing::error!(error = %e, "failed to dismiss redacted notification");
        }
    }
}
