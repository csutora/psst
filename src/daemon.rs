use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use matrix_sdk::ruma::events::receipt::{ReceiptEventContent, ReceiptType};
use matrix_sdk::ruma::events::SyncEphemeralRoomEvent;
use matrix_sdk::ruma::events::room::message::SyncRoomMessageEvent;
use matrix_sdk::ruma::push::{Action, Tweak};
use matrix_sdk::ruma::OwnedEventId;
use matrix_sdk::Room;
use matrix_sdk_ui::sync_service::{State as SyncState, SyncService};
use tokio::sync::{watch, Mutex};

use crate::config::Config;
use crate::filters::{self, EventContext, FilterResult};
use crate::notification::{self, Notification, Notifier};
use crate::session;

pub async fn run(data_dir: &Path, config: Config, config_path: &Path) -> anyhow::Result<()> {
    let client = match session::restore_session(data_dir).await {
        Ok(c) => c,
        Err(_) => {
            tracing::info!("no session found, run `psst login` first");
            return Ok(());
        }
    };

    let user_id = client
        .user_id()
        .context("no user ID after session restore")?
        .to_owned();

    tracing::info!(user_id = %user_id, "session restored, building sync service");

    let notifier: Arc<dyn Notifier> = Arc::from(notification::create_notifier()?);

    // config watch channel: handlers always see the latest config
    let (config_tx, config_rx) = watch::channel(Arc::new(config));

    // track whether initial sync has completed (suppress notifications until then)
    let initial_sync_done = Arc::new(AtomicBool::new(false));

    // dedup set: SyncService's dual connections can dispatch the same event twice
    let seen_events: Arc<Mutex<HashSet<OwnedEventId>>> = Arc::new(Mutex::new(HashSet::new()));

    // register event handler for room messages
    {
        let initial_sync_done = initial_sync_done.clone();
        let notifier = notifier.clone();
        let config_rx = config_rx.clone();
        let own_user_id = user_id.clone();
        let seen_events = seen_events.clone();

        client.add_event_handler(
            move |ev: SyncRoomMessageEvent, room: Room, actions: Vec<Action>| {
                let initial_sync_done = initial_sync_done.clone();
                let notifier = notifier.clone();
                let config_rx = config_rx.clone();
                let own_user_id = own_user_id.clone();
                let seen_events = seen_events.clone();
                async move {
                    let config = config_rx.borrow().clone();
                    handle_message(ev, room, actions, &own_user_id, &initial_sync_done, &seen_events, &*notifier, &config).await;
                }
            },
        );
    }

    // register event handler for read receipts (dismiss notifications)
    {
        let initial_sync_done = initial_sync_done.clone();
        let notifier = notifier.clone();
        let own_user_id = user_id.clone();

        client.add_event_handler(
            move |ev: SyncEphemeralRoomEvent<ReceiptEventContent>, room: Room| {
                let initial_sync_done = initial_sync_done.clone();
                let notifier = notifier.clone();
                let own_user_id = own_user_id.clone();
                async move {
                    handle_receipt(ev, room, &own_user_id, &initial_sync_done, &*notifier).await;
                }
            },
        );
    }

    // build and start SyncService
    let sync_service = SyncService::builder(client.clone())
        .build()
        .await
        .context("failed to build SyncService")?;

    // monitor sync state changes and restart on error
    let state_sync_flag = initial_sync_done.clone();
    let sync_service_handle = Arc::new(sync_service);
    let state_handle = sync_service_handle.clone();
    let mut state_subscriber = state_handle.state();
    let state_task = tokio::spawn(async move {
        let mut first_running = true;
        loop {
            let state = state_subscriber.next().await;
            match state {
                Some(SyncState::Running) => {
                    if first_running {
                        first_running = false;
                        state_sync_flag.store(true, Ordering::Relaxed);
                        tracing::info!("initial sync complete, now monitoring for notifications");
                    } else {
                        tracing::info!("sync resumed");
                    }
                }
                Some(SyncState::Error(e)) => {
                    let msg = e.to_string();
                    if msg.contains("M_UNKNOWN_TOKEN") {
                        tracing::error!(error = %e, "session invalidated, shutting down");
                        break;
                    }
                    tracing::warn!(error = %e, "sync error, restarting in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    state_handle.start().await;
                }
                Some(SyncState::Terminated) => {
                    tracing::info!("sync terminated");
                    break;
                }
                Some(other) => {
                    tracing::debug!(?other, "sync state changed");
                }
                None => break,
            }
        }
    });

    sync_service_handle.start().await;

    let rooms = client.joined_rooms();
    let encrypted_count = rooms.iter().filter(|r| r.encryption_state().is_encrypted()).count();
    tracing::info!(
        rooms = rooms.len(),
        encrypted = encrypted_count,
        unencrypted = rooms.len() - encrypted_count,
        "daemon started, waiting for events"
    );
    if encrypted_count > 0 {
        let backup_enabled = matches!(
            client.encryption().backups().state(),
            matrix_sdk::encryption::backups::BackupState::Enabled
        );
        if !backup_enabled {
            tracing::warn!(
                encrypted = encrypted_count,
                "encrypted rooms require device verification + key import (psst verify)"
            );
        }
    }

    // config file watcher: reload on changes
    let config_path_owned = config_path.to_path_buf();
    let (fs_reload_tx, mut fs_reload_rx) = tokio::sync::mpsc::channel::<()>(1);
    let _watcher = start_config_watcher(&config_path_owned, fs_reload_tx.clone())?;

    // wait for shutdown signal
    let mut signals = signal_hook_tokio::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ])
    .context("failed to register signal handlers")?;

    loop {
        tokio::select! {
            Some(signal) = signals.next() => {
                match signal {
                    signal_hook::consts::SIGHUP => {
                        tracing::info!("SIGHUP received, reloading config");
                        reload_config(&config_path_owned, &config_tx);
                    }
                    _ => {
                        tracing::info!(signal, "shutdown signal received");
                        break;
                    }
                }
            }
            Some(()) = fs_reload_rx.recv() => {
                // debounce: drain any additional events that arrived
                while fs_reload_rx.try_recv().is_ok() {}
                tracing::info!("config file changed, reloading");
                reload_config(&config_path_owned, &config_tx);
            }
            else => break,
        }
    }

    tracing::info!("stopping sync service");
    sync_service_handle.stop().await;
    state_task.abort();

    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;

    tracing::info!("daemon stopped");
    Ok(())
}

fn start_config_watcher(
    config_path: &Path,
    tx: tokio::sync::mpsc::Sender<()>,
) -> anyhow::Result<notify::RecommendedWatcher> {
    use notify::{Watcher, RecursiveMode, recommended_watcher, Event as FsEvent};
    let mut watcher = recommended_watcher(move |res: Result<FsEvent, notify::Error>| {
        if let Ok(event) = res {
            if event.kind.is_modify() || event.kind.is_create() {
                let _ = tx.try_send(());
            }
        }
    })
    .context("failed to create config file watcher")?;
    // watch the parent directory (some editors do atomic writes via rename)
    let watch_dir = config_path.parent().unwrap_or(Path::new("."));
    if watch_dir.exists() {
        let _ = watcher.watch(watch_dir, RecursiveMode::NonRecursive);
        tracing::info!(path = %watch_dir.display(), "watching config directory for changes");
    }
    Ok(watcher)
}

fn reload_config(path: &Path, tx: &watch::Sender<Arc<Config>>) {
    match Config::load(path) {
        Ok(new_config) => {
            tracing::info!("config reloaded successfully");
            let _ = tx.send(Arc::new(new_config));
        }
        Err(e) => {
            tracing::error!(error = %e, "config reload failed, keeping previous config");
        }
    }
}

async fn handle_message(
    ev: SyncRoomMessageEvent,
    room: Room,
    actions: Vec<Action>,
    own_user_id: &matrix_sdk::ruma::OwnedUserId,
    initial_sync_done: &AtomicBool,
    seen_events: &Mutex<HashSet<OwnedEventId>>,
    notifier: &dyn Notifier,
    config: &Config,
) {
    // skip own messages
    if ev.sender() == own_user_id {
        return;
    }

    // suppress during initial sync
    if !initial_sync_done.load(Ordering::Relaxed) {
        return;
    }

    // deduplicate: SyncService dispatches from both room-list and encryption connections
    {
        let mut seen = seen_events.lock().await;
        if !seen.insert(ev.event_id().to_owned()) {
            tracing::debug!(event_id = %ev.event_id(), "skipping duplicate event");
            return;
        }
        // cap the set size to avoid unbounded growth
        if seen.len() > 10_000 {
            seen.clear();
        }
    }

    // get original event content (skip redacted events)
    let original = match ev.as_original() {
        Some(o) => o,
        None => return,
    };

    // check push rules from server
    let push_notify = actions.iter().any(|a| matches!(a, Action::Notify));
    let push_highlight = actions
        .iter()
        .any(|a| matches!(a, Action::SetTweak(Tweak::Highlight(true))));

    // check for edits (m.replace relation)
    let is_edit = original
        .content
        .relates_to
        .as_ref()
        .map_or(false, |r| r.rel_type().map_or(false, |t| t.as_str() == "m.replace"));

    // room info
    let is_direct = room.is_direct().await.unwrap_or(false);
    let is_encrypted = room.encryption_state().is_encrypted();

    let event_ts_millis: u64 = original.origin_server_ts.0.into();
    let event_ts_secs = (event_ts_millis / 1000) as i64;

    let ctx = EventContext {
        sender: ev.sender().to_string(),
        room_id: room.room_id().to_string(),
        is_direct,
        is_encrypted,
        is_edit,
        push_notify,
        push_highlight,
        event_ts_secs,
    };

    tracing::info!(
        room_id = %room.room_id(),
        sender = %ev.sender(),
        is_direct,
        is_encrypted,
        push_notify,
        push_highlight,
        "received message"
    );

    let sound = match filters::evaluate(&ctx, config) {
        FilterResult::Notify { sound } => sound,
        FilterResult::Suppress => {
            tracing::debug!(
                room_id = %room.room_id(),
                sender = %ev.sender(),
                "suppressed by filters"
            );
            return;
        }
    };

    // build notification
    let room_name = room
        .display_name()
        .await
        .map(|n| n.to_string())
        .unwrap_or_else(|_| room.room_id().to_string());

    let sender_name = match room.get_member(ev.sender()).await {
        Ok(Some(member)) => member
            .display_name()
            .unwrap_or_else(|| ev.sender().localpart())
            .to_string(),
        _ => ev.sender().localpart().to_string(),
    };

    let body = filters::format_body(&original.content.msgtype, &sender_name, config);

    let (title, subtitle) = if is_direct {
        (sender_name, None)
    } else {
        (room_name, Some(sender_name))
    };

    let notification = Notification {
        tag: room.room_id().to_string(),
        title,
        subtitle,
        body,
        sound,
        thread_id: room.room_id().to_string(),
    };

    if let Err(e) = notifier.send(&notification) {
        tracing::error!(error = %e, "failed to send notification");
    }
}

async fn handle_receipt(
    ev: SyncEphemeralRoomEvent<ReceiptEventContent>,
    room: Room,
    own_user_id: &matrix_sdk::ruma::OwnedUserId,
    initial_sync_done: &AtomicBool,
    notifier: &dyn Notifier,
) {
    if !initial_sync_done.load(Ordering::Relaxed) {
        return;
    }

    // check if our own user sent a read receipt (from another device)
    let has_own_receipt = ev
        .content
        .user_receipt(own_user_id, ReceiptType::Read)
        .is_some()
        || ev
            .content
            .user_receipt(own_user_id, ReceiptType::ReadPrivate)
            .is_some();

    if has_own_receipt {
        let room_id = room.room_id().to_string();
        tracing::debug!(room_id = %room_id, "own read receipt received, dismissing notification");
        if let Err(e) = notifier.dismiss(&room_id) {
            tracing::error!(error = %e, "failed to dismiss notification");
        }
    }
}
