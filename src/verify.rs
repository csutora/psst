use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context};
use futures_util::StreamExt;
use matrix_sdk::encryption::verification::{SasState, SasVerification, VerificationRequestState};
use matrix_sdk::Client;
use matrix_sdk_ui::sync_service::{State as SyncState, SyncService};

use crate::session;

/// start sync service and wait for initial sync to complete
async fn start_sync(client: &Client) -> anyhow::Result<SyncService> {
    let sync_service = SyncService::builder(client.clone())
        .build()
        .await
        .context("failed to build sync service")?;

    // subscribe before starting so we don't miss the Running state
    let mut state_sub = sync_service.state();
    sync_service.start().await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::select! {
            state = state_sub.next() => {
                match state {
                    Some(SyncState::Running) => return Ok(sync_service),
                    Some(SyncState::Error(e)) => bail!("sync error: {e}"),
                    Some(SyncState::Terminated) => bail!("sync terminated unexpectedly"),
                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                bail!("timed out waiting for initial sync");
            }
        }
    }
}

/// interactive emoji verification, then key backup import
pub async fn verify(data_dir: &Path) -> anyhow::Result<()> {
    let client = session::restore_session(data_dir).await?;
    let sync_service = start_sync(&client).await?;

    // run the actual verification, but always clean up sync on exit
    let result = do_verify(&client).await;

    sync_service.stop().await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    result
}

async fn do_verify(client: &Client) -> anyhow::Result<()> {
    let user_id = client
        .user_id()
        .context("no user ID")?
        .to_owned();

    let own_identity = client
        .encryption()
        .get_user_identity(&user_id)
        .await?
        .context("could not find own user identity. is cross-signing set up?")?;

    // try up to 3 times; stale requests from other clients can cause
    // the sdk to cancel concurrent verification flows
    for attempt in 0..3 {
        eprintln!("requesting verification from your other devices...");
        eprintln!("accept the request on a verified session.");
        eprintln!();

        let request = own_identity
            .request_verification()
            .await
            .context("failed to send verification request")?;

        let mut changes = request.changes();
        let mut should_retry = false;

        while let Some(state) = changes.next().await {
            match state {
                VerificationRequestState::Ready { .. } => {
                    eprintln!("request accepted, waiting for the other device to start emoji comparison...");
                }
                VerificationRequestState::Transitioned { verification } => {
                    let sas = verification
                        .sas()
                        .context("non-SAS method not supported")?;
                    run_sas(sas.clone(), true).await?;

                    eprintln!();
                    import_keys_inner(client).await?;
                    return Ok(());
                }
                VerificationRequestState::Done => {
                    eprintln!("verification completed.");
                    eprintln!();
                    import_keys_inner(client).await?;
                    return Ok(());
                }
                VerificationRequestState::Cancelled(_) if attempt < 2 => {
                    eprintln!("conflicted with a stale verification request, retrying...");
                    eprintln!();
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    should_retry = true;
                    break;
                }
                VerificationRequestState::Cancelled(info) => {
                    bail!("verification cancelled: {}", info.reason());
                }
                _ => {}
            }
        }

        if !should_retry {
            bail!("verification request stream ended unexpectedly");
        }
    }

    bail!("verification failed after multiple retries")
}

/// standalone key backup import (no verification)
pub async fn import_keys(data_dir: &Path) -> anyhow::Result<()> {
    let client = session::restore_session(data_dir).await?;
    let sync_service = start_sync(&client).await?;

    // brief pause for sync to process events
    tokio::time::sleep(Duration::from_secs(2)).await;

    let result = import_keys_inner(&client).await;

    sync_service.stop().await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    result
}

/// run the sas emoji comparison flow
/// `should_accept` is true only when we received sas from the other side
async fn run_sas(sas: SasVerification, should_accept: bool) -> anyhow::Result<()> {
    if should_accept {
        sas.accept().await.context("failed to accept SAS")?;
    }

    let mut changes = sas.changes();
    while let Some(state) = changes.next().await {
        match state {
            SasState::KeysExchanged { emojis, .. } => {
                if let Some(emojis) = emojis {
                    eprintln!();
                    eprintln!("compare these emoji on both devices:");
                    eprintln!();
                    for emoji in emojis.emojis.iter() {
                        eprintln!("  {}  {}", emoji.symbol, emoji.description);
                    }
                    eprintln!();
                    eprint!("do they match? [y/n]: ");

                    let mut input = String::new();
                    std::io::stdin()
                        .read_line(&mut input)
                        .context("failed to read input")?;

                    if input.trim().eq_ignore_ascii_case("y") {
                        sas.confirm().await.context("failed to confirm SAS")?;
                        eprintln!("confirmed, waiting for other device...");
                    } else {
                        sas.mismatch().await.context("failed to report mismatch")?;
                        bail!("emoji mismatch, verification cancelled");
                    }
                }
            }
            SasState::Done { .. } => {
                eprintln!("verification successful!");
                break;
            }
            SasState::Cancelled(info) => {
                bail!("verification cancelled: {}", info.reason());
            }
            _ => {}
        }
    }
    Ok(())
}

/// shared key import logic: prompt for recovery key and recover from backup
async fn import_keys_inner(client: &Client) -> anyhow::Result<()> {
    use matrix_sdk::encryption::backups::BackupState;
    use matrix_sdk::encryption::recovery::RecoveryState;

    let recovery = client.encryption().recovery();
    let backups = client.encryption().backups();

    // after verification, the other device may have sent us the backup key
    // via secret sharing; wait briefly for that to arrive
    for _ in 0..10 {
        if matches!(backups.state(), BackupState::Enabled) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // if backups are already enabled (via secret sharing), we're done
    if matches!(backups.state(), BackupState::Enabled) {
        eprintln!("key backup enabled, encrypted rooms should now decrypt.");
        return Ok(());
    }

    if matches!(recovery.state(), RecoveryState::Enabled) {
        eprintln!("recovery already enabled, keys are available.");
        return Ok(());
    }

    // check if backup exists on server
    let has_backup = backups
        .fetch_exists_on_server()
        .await
        .unwrap_or(false);

    if !has_backup {
        eprintln!("no server-side key backup found.");
        return Ok(());
    }

    if matches!(recovery.state(), RecoveryState::Disabled) {
        eprintln!("no secret storage found on server.");
        return Ok(());
    }

    eprintln!("backup key not received via secret sharing.");
    eprintln!("enter your recovery key:");
    let key = rpassword::prompt_password("recovery key: ")
        .context("failed to read recovery key")?;

    eprintln!("importing keys from backup...");

    recovery
        .recover(key.trim())
        .await
        .context("failed to recover from key backup. is the recovery key correct?")?;

    eprintln!("key import complete! encrypted rooms should now decrypt.");
    Ok(())
}
