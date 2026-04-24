use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use matrix_sdk::ruma::events::call::hangup::SyncCallHangupEvent;
use matrix_sdk::ruma::events::call::invite::SyncCallInviteEvent;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::receipt::ReceiptEventContent;
use matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::SyncRoomMessageEvent;
use matrix_sdk::ruma::events::room::redaction::SyncRoomRedactionEvent;
use matrix_sdk::ruma::events::SyncEphemeralRoomEvent;
use matrix_sdk::ruma::events::SyncMessageLikeEvent;
use matrix_sdk::ruma::push::Action;
use matrix_sdk::Room;
use matrix_sdk_ui::sync_service::{State as SyncState, SyncService};
use tokio::sync::watch;

use crate::config::Config;
use crate::notification::{self, Notifier};
use crate::session;

use super::handlers::{
    handle_call_hangup, handle_call_invite, handle_invite, handle_message, handle_reaction,
    handle_receipt, handle_redaction,
};
use super::state::{DaemonState, NOTIF_CLEANUP_INTERVAL};

/// backoff before restarting the sync service after a transient error
const SYNC_RETRY_DELAY: Duration = Duration::from_secs(2);

/// brief grace period after `drop(client)` so matrix-sdk's background tasks
/// can notice the drop and flush pending IO before we return
const SHUTDOWN_FLUSH_DELAY: Duration = Duration::from_millis(100);

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

    let (config_tx, config_rx) = watch::channel(Arc::new(config));

    let state = Arc::new(DaemonState::new(user_id.clone()));

    // fetch own display name in the background; cache it for mention detection
    let display_name_task = {
        let state = state.clone();
        let client = client.clone();
        tokio::spawn(async move {
            match client.account().get_display_name().await {
                Ok(Some(dn)) => {
                    *state.display_name.write().await = Some(dn);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "could not fetch own display name (mention fallback degraded)");
                }
            }
        })
    };

    // periodic cleanup of expired notification history entries
    let cleanup_task = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(NOTIF_CLEANUP_INTERVAL);
            ticker.tick().await; // skip immediate first tick
            loop {
                ticker.tick().await;
                state.cleanup_expired_notifs().await;
            }
        })
    };

    register_handlers(&client, state.clone(), notifier.clone(), config_rx.clone());

    let sync_service = SyncService::builder(client.clone())
        .build()
        .await
        .context("failed to build SyncService")?;

    let state_for_task = state.clone();
    let sync_service_handle = Arc::new(sync_service);
    let state_handle = sync_service_handle.clone();
    let mut state_subscriber = state_handle.state();
    let state_task = tokio::spawn(async move {
        let mut first_running = true;
        loop {
            let s = state_subscriber.next().await;
            match s {
                Some(SyncState::Running) => {
                    if first_running {
                        first_running = false;
                        state_for_task.initial_sync_done.store(true, Ordering::Relaxed);
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
                    tokio::time::sleep(SYNC_RETRY_DELAY).await;
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

    // validate room overrides against actually-joined rooms
    validate_overrides_against_joined(&client, &config_rx.borrow());

    let config_path_owned = config_path.to_path_buf();
    let (fs_reload_tx, mut fs_reload_rx) = tokio::sync::mpsc::channel::<()>(1);
    let _watcher = start_config_watcher(&config_path_owned, fs_reload_tx.clone())?;

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
                        if reload_config(&config_path_owned, &config_tx).is_ok() {
                            validate_overrides_against_joined(&client, &config_rx.borrow());
                        }
                    }
                    _ => {
                        tracing::info!(signal, "shutdown signal received");
                        break;
                    }
                }
            }
            Some(()) = fs_reload_rx.recv() => {
                while fs_reload_rx.try_recv().is_ok() {}
                tracing::info!("config file changed, reloading");
                if reload_config(&config_path_owned, &config_tx).is_ok() {
                    validate_overrides_against_joined(&client, &config_rx.borrow());
                }
            }
            else => break,
        }
    }

    tracing::info!("stopping sync service");
    sync_service_handle.stop().await;
    state_task.abort();
    cleanup_task.abort();
    display_name_task.abort();

    drop(client);
    tokio::time::sleep(SHUTDOWN_FLUSH_DELAY).await;

    tracing::info!("daemon stopped");
    Ok(())
}

fn register_handlers(
    client: &matrix_sdk::Client,
    state: Arc<DaemonState>,
    notifier: Arc<dyn Notifier>,
    config_rx: watch::Receiver<Arc<Config>>,
) {
    // messages (and edits)
    {
        let state = state.clone();
        let notifier = notifier.clone();
        let config_rx = config_rx.clone();
        client.add_event_handler(
            move |ev: SyncRoomMessageEvent, room: Room, actions: Vec<Action>| {
                let state = state.clone();
                let notifier = notifier.clone();
                let config_rx = config_rx.clone();
                async move {
                    let config = config_rx.borrow().clone();
                    handle_message(ev, room, actions, &state, &*notifier, &config).await;
                }
            },
        );
    }

    // read receipts (dismiss notifications)
    {
        let state = state.clone();
        let notifier = notifier.clone();
        client.add_event_handler(
            move |ev: SyncEphemeralRoomEvent<ReceiptEventContent>, room: Room| {
                let state = state.clone();
                let notifier = notifier.clone();
                async move {
                    handle_receipt(ev, room, &state, &*notifier).await;
                }
            },
        );
    }

    // invites
    {
        let state = state.clone();
        let notifier = notifier.clone();
        let config_rx = config_rx.clone();
        client.add_event_handler(
            move |ev: StrippedRoomMemberEvent, room: Room| {
                let state = state.clone();
                let notifier = notifier.clone();
                let config_rx = config_rx.clone();
                async move {
                    let config = config_rx.borrow().clone();
                    handle_invite(ev, room, &state, &*notifier, &config).await;
                }
            },
        );
    }

    // reactions
    {
        let state = state.clone();
        let notifier = notifier.clone();
        let config_rx = config_rx.clone();
        client.add_event_handler(
            move |ev: SyncMessageLikeEvent<ReactionEventContent>, room: Room| {
                let state = state.clone();
                let notifier = notifier.clone();
                let config_rx = config_rx.clone();
                async move {
                    let config = config_rx.borrow().clone();
                    handle_reaction(ev, room, &state, &*notifier, &config).await;
                }
            },
        );
    }

    // legacy 1:1 calls
    {
        let state = state.clone();
        let notifier = notifier.clone();
        let config_rx = config_rx.clone();
        client.add_event_handler(
            move |ev: SyncCallInviteEvent, room: Room| {
                let state = state.clone();
                let notifier = notifier.clone();
                let config_rx = config_rx.clone();
                async move {
                    let config = config_rx.borrow().clone();
                    handle_call_invite(ev, room, &state, &*notifier, &config).await;
                }
            },
        );
    }

    // call hangup → dismiss the corresponding "incoming call" notification
    {
        let state = state.clone();
        let notifier = notifier.clone();
        client.add_event_handler(
            move |ev: SyncCallHangupEvent, room: Room| {
                let state = state.clone();
                let notifier = notifier.clone();
                async move {
                    handle_call_hangup(ev, room, &state, &*notifier).await;
                }
            },
        );
    }

    // redactions → dismiss the notification for the redacted event
    {
        let state = state.clone();
        let notifier = notifier.clone();
        client.add_event_handler(
            move |ev: SyncRoomRedactionEvent, room: Room| {
                let state = state.clone();
                let notifier = notifier.clone();
                async move {
                    handle_redaction(ev, room, &state, &*notifier).await;
                }
            },
        );
    }
}

fn start_config_watcher(
    config_path: &Path,
    tx: tokio::sync::mpsc::Sender<()>,
) -> anyhow::Result<notify::RecommendedWatcher> {
    use notify::{recommended_watcher, Event as FsEvent, RecursiveMode, Watcher};
    let mut watcher = recommended_watcher(move |res: Result<FsEvent, notify::Error>| {
        if let Ok(event) = res {
            if event.kind.is_modify() || event.kind.is_create() {
                let _ = tx.try_send(());
            }
        }
    })
    .context("failed to create config file watcher")?;
    let watch_dir = config_path.parent().unwrap_or(Path::new("."));
    if watch_dir.exists() {
        let _ = watcher.watch(watch_dir, RecursiveMode::NonRecursive);
        tracing::info!(path = %watch_dir.display(), "watching config directory for changes");
    }
    Ok(watcher)
}

fn reload_config(path: &Path, tx: &watch::Sender<Arc<Config>>) -> anyhow::Result<()> {
    match Config::load(path) {
        Ok(new_config) => {
            tracing::info!("config reloaded successfully");
            let _ = tx.send(Arc::new(new_config));
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "config reload failed, keeping previous config");
            Err(e)
        }
    }
}

fn validate_overrides_against_joined(client: &matrix_sdk::Client, config: &Config) {
    let joined: HashSet<String> = client
        .joined_rooms()
        .iter()
        .map(|r| r.room_id().to_string())
        .collect();
    for room_id in config.notifications.rooms.overrides.keys() {
        if !joined.contains(room_id) {
            tracing::warn!(
                "room override `{room_id}` is not a joined room. did you copy the wrong id from `psst list-rooms`?"
            );
        }
    }
}
