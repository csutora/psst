use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use matrix_sdk::{
    authentication::matrix::MatrixSession, Client, SessionMeta,
};
use serde::{Deserialize, Serialize};

/// how credentials were stored at login time
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialStorage {
    /// secrets stored in system keyring
    Keyring,
    /// secrets stored in a local file with restrictive permissions (0600)
    File,
}

/// non-secret session metadata persisted to `session.json`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub homeserver_url: String,
    pub user_id: String,
    pub device_id: String,
    pub db_path: String,
    pub credential_storage: CredentialStorage,
}

/// secrets that need secure storage
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCredentials {
    /// serialized MatrixSession (contains access token, etc)
    session_json: String,
    /// passphrase for the state store encryption
    db_passphrase: String,
}

const KEYRING_SERVICE: &str = "psst";

fn keyring_entry(user_id: &str) -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("credentials:{user_id}"))
        .context("Failed to create keyring entry")
}

/// check if the system keyring is available by attempting to create an entry
fn is_keyring_available() -> bool {
    match keyring::Entry::new(KEYRING_SERVICE, "probe") {
        Ok(entry) => {
            // try a get; NoEntry is fine (means keyring works, just empty)
            // a PlatformFailure or NoStorageAccess means keyring is unavailable
            match entry.get_password() {
                Ok(_) => true,
                Err(keyring::Error::NoEntry) => true,
                Err(keyring::Error::PlatformFailure(_)) => false,
                Err(keyring::Error::NoStorageAccess(_)) => false,
                Err(_) => true, // other errors likely mean it's accessible but something else went wrong
            }
        }
        Err(_) => false,
    }
}

/// store credentials in the system keyring
fn store_in_keyring(user_id: &str, creds: &StoredCredentials) -> anyhow::Result<()> {
    let entry = keyring_entry(user_id)?;
    let json = serde_json::to_string(creds).context("Failed to serialize credentials")?;
    entry
        .set_password(&json)
        .context("Failed to store credentials in keyring")?;
    tracing::info!("Credentials stored in system keyring");
    Ok(())
}

/// retrieve credentials from the system keyring
fn load_from_keyring(user_id: &str) -> anyhow::Result<StoredCredentials> {
    let entry = keyring_entry(user_id)?;
    let json = entry
        .get_password()
        .context("Failed to retrieve credentials from keyring")?;
    serde_json::from_str(&json).context("Failed to parse credentials from keyring")
}

/// delete credentials from the system keyring
fn delete_from_keyring(user_id: &str) -> anyhow::Result<()> {
    let entry = keyring_entry(user_id)?;
    match entry.delete_credential() {
        Ok(()) => {
            tracing::info!("Credentials removed from keyring");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            tracing::debug!("No keyring entry to delete");
            Ok(())
        }
        Err(e) => Err(e).context("Failed to delete credentials from keyring"),
    }
}

/// store credentials in a file with restrictive permissions
fn store_in_file(data_dir: &Path, creds: &StoredCredentials) -> anyhow::Result<()> {
    let path = data_dir.join("credentials.json");
    let json = serde_json::to_string_pretty(creds).context("Failed to serialize credentials")?;
    std::fs::write(&path, &json)
        .with_context(|| format!("Failed to write credentials to {}", path.display()))?;

    // set file permissions to 0600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .context("Failed to set restrictive permissions on credentials file")?;
    }

    tracing::warn!("Credentials stored in file (less secure than keyring)");
    Ok(())
}

/// load credentials from file
fn load_from_file(data_dir: &Path) -> anyhow::Result<StoredCredentials> {
    let path = data_dir.join("credentials.json");
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read credentials from {}", path.display()))?;
    serde_json::from_str(&json).context("Failed to parse credentials file")
}

/// delete credentials file
fn delete_cred_file(data_dir: &Path) -> anyhow::Result<()> {
    let path = data_dir.join("credentials.json");
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to delete {}", path.display()))?;
        tracing::info!("Credentials file removed");
    }
    Ok(())
}

/// paths for session files within the data directory
fn session_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("session.json")
}

fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("store")
}

/// save session metadata to disk
fn save_metadata(data_dir: &Path, meta: &SessionMetadata) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("Failed to create data directory: {}", data_dir.display()))?;
    let path = session_file_path(data_dir);
    let json = serde_json::to_string_pretty(meta).context("Failed to serialize session metadata")?;
    std::fs::write(&path, &json)
        .with_context(|| format!("Failed to write session metadata to {}", path.display()))?;
    tracing::info!(path = %path.display(), "Session metadata saved");
    Ok(())
}

/// load session metadata from disk
pub fn load_metadata(data_dir: &Path) -> anyhow::Result<Option<SessionMetadata>> {
    let path = session_file_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read session metadata from {}", path.display()))?;
    let meta: SessionMetadata =
        serde_json::from_str(&json).context("Failed to parse session metadata")?;
    Ok(Some(meta))
}

/// generate a random passphrase for store encryption
fn generate_passphrase() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..64)
        .map(|_| rng.sample(rand::distributions::Alphanumeric) as char)
        .collect()
}

/// build a client with the sqlite store, using a known homeserver url
/// used for session restore when we already know the resolved homeserver
pub async fn build_client_with_url(
    homeserver_url: &str,
    data_dir: &Path,
    db_passphrase: &str,
) -> anyhow::Result<Client> {
    let db_path = store_path(data_dir);
    std::fs::create_dir_all(&db_path)
        .with_context(|| format!("Failed to create store directory: {}", db_path.display()))?;

    let client = Client::builder()
        .homeserver_url(homeserver_url)
        .sqlite_store(&db_path, Some(db_passphrase))
        .build()
        .await
        .context("Failed to build Matrix client")?;

    Ok(client)
}

/// build a client with the sqlite store, using server name discovery
/// performs .well-known auto-discovery if the input is a server name
pub async fn build_client_with_discovery(
    server_name_or_url: &str,
    data_dir: &Path,
    db_passphrase: &str,
) -> anyhow::Result<Client> {
    let db_path = store_path(data_dir);
    std::fs::create_dir_all(&db_path)
        .with_context(|| format!("Failed to create store directory: {}", db_path.display()))?;

    let client = Client::builder()
        .server_name_or_homeserver_url(server_name_or_url)
        .sqlite_store(&db_path, Some(db_passphrase))
        .build()
        .await
        .context("Failed to build Matrix client (check homeserver URL or server name)")?;

    Ok(client)
}

/// restore a previously saved session, returns the client ready for use
pub async fn restore_session(data_dir: &Path) -> anyhow::Result<Client> {
    let meta = load_metadata(data_dir)?
        .context("No session found. Run `psst login` first.")?;

    let creds = match meta.credential_storage {
        CredentialStorage::Keyring => load_from_keyring(&meta.user_id)
            .context("Failed to load credentials from keyring")?,
        CredentialStorage::File => {
            tracing::warn!("Loading credentials from file (less secure)");
            load_from_file(data_dir)
                .context("Failed to load credentials from file")?
        }
    };

    let client = build_client_with_url(&meta.homeserver_url, data_dir, &creds.db_passphrase).await?;

    let session: MatrixSession = serde_json::from_str(&creds.session_json)
        .context("Failed to deserialize stored Matrix session")?;

    client
        .restore_session(session)
        .await
        .context("Failed to restore Matrix session")?;

    tracing::info!(
        user_id = %meta.user_id,
        device_id = %meta.device_id,
        "Session restored"
    );

    Ok(client)
}

/// interactive login flow
pub async fn login(
    data_dir: &Path,
    homeserver: Option<&str>,
    username: Option<&str>,
) -> anyhow::Result<()> {
    // check for existing session
    if let Some(meta) = load_metadata(data_dir)? {
        bail!(
            "Already logged in as {}. Run `psst logout` first.",
            meta.user_id
        );
    }

    // prompt for homeserver if not provided
    let server_name_or_url = match homeserver {
        Some(url) => url.to_string(),
        None => {
            eprint!("homeserver URL or server name: ");
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("Failed to read homeserver")?;
            input.trim().to_string()
        }
    };

    // prompt for username if not provided
    let username_str = match username {
        Some(u) => u.to_string(),
        None => {
            eprint!("username: ");
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("Failed to read username")?;
            input.trim().to_string()
        }
    };

    // prompt for password (echo disabled)
    let password =
        rpassword::prompt_password("password: ").context("Failed to read password")?;

    // generate db passphrase
    let db_passphrase = generate_passphrase();

    // clean any stale store from a previous failed login attempt
    let db_path = store_path(data_dir);
    if db_path.exists() {
        tracing::debug!("Removing stale store from previous failed login");
        let _ = std::fs::remove_dir_all(&db_path);
    }

    // build client with auto-discovery and login
    tracing::info!(server = %server_name_or_url, "Connecting to homeserver");
    let client = match build_client_with_discovery(&server_name_or_url, data_dir, &db_passphrase).await {
        Ok(c) => c,
        Err(e) => {
            // clean up store on failure so next attempt starts fresh
            let _ = std::fs::remove_dir_all(store_path(data_dir));
            return Err(e);
        }
    };

    let device_name = format!(
        "psst on {}",
        hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    );

    let matrix_auth = client.matrix_auth();
    if let Err(e) = matrix_auth
        .login_username(&username_str, &password)
        .initial_device_display_name(&device_name)
        .await
    {
        // clean up store on login failure
        let _ = std::fs::remove_dir_all(store_path(data_dir));
        return Err(anyhow::anyhow!(e).context("Login failed"));
    }

    let session = matrix_auth
        .session()
        .context("Login succeeded but no session returned")?;

    let session_meta: &SessionMeta = client
        .session_meta()
        .context("No session meta after login")?;

    let user_id = session_meta.user_id.to_string();
    let device_id = session_meta.device_id.to_string();

    tracing::info!(
        user_id = %user_id,
        device_id = %device_id,
        device_name = %device_name,
        "Login successful"
    );

    // serialize the MatrixSession for storage
    let session_json =
        serde_json::to_string(&session).context("Failed to serialize session")?;

    let creds = StoredCredentials {
        session_json,
        db_passphrase,
    };

    // determine credential storage
    let credential_storage = if is_keyring_available() {
        store_in_keyring(&user_id, &creds)?;
        CredentialStorage::Keyring
    } else {
        eprintln!();
        eprintln!("system keyring is not available. credentials cannot be stored securely.");
        eprintln!();
        eprintln!("options:");
        eprintln!("  1. don't save credentials (you'll need to run `psst login` before each daemon start)");
        eprintln!("  2. save to file with restrictive permissions (less secure)");
        eprint!("choice [1/2]: ");

        let mut choice = String::new();
        std::io::stdin()
            .read_line(&mut choice)
            .context("Failed to read choice")?;

        match choice.trim() {
            "2" => {
                store_in_file(data_dir, &creds)?;
                CredentialStorage::File
            }
            _ => {
                eprintln!("credentials will not be saved. run `psst login` before starting the daemon.");
                // still save metadata so status knows about the session,
                // but credentials won't be available after restart
                CredentialStorage::Keyring // technically keyring, but it won't find anything
            }
        }
    };

    // save non-secret metadata (use the resolved homeserver url, not the input)
    let homeserver_url = client.homeserver().to_string();
    let meta = SessionMetadata {
        homeserver_url,
        user_id: user_id.clone(),
        device_id: device_id.clone(),
        db_path: store_path(data_dir).to_string_lossy().to_string(),
        credential_storage,
    };
    save_metadata(data_dir, &meta)?;

    // drop the client and give the connection pool time to shut down
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;

    eprintln!();
    eprintln!("logged in as {user_id} (session: {device_id})");
    eprintln!();
    eprintln!("next steps:");
    eprintln!("  1. psst verify       cross-sign this device and import key backup");
    eprintln!("  2. psst test-notify  confirm notifications work");
    eprintln!("  3. psst daemon       start the notification daemon");

    Ok(())
}

/// logout: destroy local session, deactivate device on server
pub async fn logout(data_dir: &Path) -> anyhow::Result<()> {
    let meta = load_metadata(data_dir)?
        .context("Not logged in. Nothing to do.")?;

    // try server-side logout using a lightweight client (no full session restore,
    // which would trigger noisy background encryption tasks)
    let server_logout_ok = match try_server_logout(data_dir, &meta).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Server-side logout failed: {e}");
            eprintln!(
                "could not remove session from server. \
                 you may need to remove session {} manually from another client.",
                meta.device_id
            );
            false
        }
    };

    // clean up credentials
    match meta.credential_storage {
        CredentialStorage::Keyring => {
            if let Err(e) = delete_from_keyring(&meta.user_id) {
                tracing::warn!("Failed to clean keyring: {e}");
            }
        }
        CredentialStorage::File => {
            if let Err(e) = delete_cred_file(data_dir) {
                tracing::warn!("Failed to clean credentials file: {e}");
            }
        }
    }

    // remove session metadata
    let session_path = session_file_path(data_dir);
    if session_path.exists() {
        std::fs::remove_file(&session_path)
            .with_context(|| format!("Failed to remove {}", session_path.display()))?;
    }

    // remove state store
    let store = store_path(data_dir);
    if store.exists() {
        std::fs::remove_dir_all(&store)
            .with_context(|| format!("Failed to remove store at {}", store.display()))?;
    }

    if server_logout_ok {
        eprintln!("logged out and cleaned up session for {}", meta.user_id);
    } else {
        eprintln!("local session cleaned up for {}", meta.user_id);
    }
    Ok(())
}

/// attempt server-side logout without triggering full background tasks
async fn try_server_logout(data_dir: &Path, meta: &SessionMetadata) -> anyhow::Result<()> {
    let creds = match meta.credential_storage {
        CredentialStorage::Keyring => load_from_keyring(&meta.user_id)?,
        CredentialStorage::File => load_from_file(data_dir)?,
    };

    let client = build_client_with_url(&meta.homeserver_url, data_dir, &creds.db_passphrase).await?;

    let session: MatrixSession = serde_json::from_str(&creds.session_json)
        .context("Failed to deserialize stored Matrix session")?;

    client.restore_session(session).await
        .context("Failed to restore session")?;

    client.matrix_auth().logout().await
        .context("Server rejected logout request")?;

    tracing::info!("Server-side session invalidated");
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

/// print session status
pub async fn status(data_dir: &Path) -> anyhow::Result<()> {
    let meta = match load_metadata(data_dir)? {
        Some(m) => m,
        None => {
            println!("not logged in. run `psst login` to get started.");
            return Ok(());
        }
    };

    println!("user:        {}", meta.user_id);
    println!("session ID:  {}", meta.device_id);
    println!("homeserver:  {}", meta.homeserver_url);
    println!(
        "credentials: {}",
        match meta.credential_storage {
            CredentialStorage::Keyring => "system keyring",
            CredentialStorage::File => "file (less secure)",
        }
    );
    println!("store:       {}", meta.db_path);

    Ok(())
}

/// list joined rooms with ids, names, and encryption status
pub async fn list_rooms(data_dir: &Path) -> anyhow::Result<()> {
    let client = restore_session(data_dir).await?;

    let rooms = client.joined_rooms();
    if rooms.is_empty() {
        println!("no joined rooms.");
    } else {
        println!("{:<44} {:<6} {}", "ROOM ID", "E2EE", "NAME");
        println!("{}", "-".repeat(80));
        for room in &rooms {
            let name = room
                .display_name()
                .await
                .map(|n| n.to_string())
                .unwrap_or_else(|_| "?".to_string());
            let encrypted = if room.encryption_state().is_encrypted() {
                "yes"
            } else {
                "no"
            };
            println!("{:<44} {:<6} {}", room.room_id(), encrypted, name);
        }
        println!();
        println!("{} rooms total", rooms.len());
    }

    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

/// mark a room as read and dismiss its notification
pub async fn mark_read(data_dir: &Path, room_id: &str) -> anyhow::Result<()> {
    use matrix_sdk::ruma::RoomId;

    let client = restore_session(data_dir).await?;

    let room_id = <&RoomId>::try_from(room_id)
        .context("invalid room ID format (expected !abc123:example.com)")?;

    let room = client
        .get_room(room_id)
        .context("room not found. are you a member?")?;

    // find the latest event to mark as read
    // send a read receipt for the latest event in the timeline
    let latest_event = room.latest_event();
    match latest_event {
        Some(event) => {
            let event_id = event.event_id()
                .context("latest event has no event ID")?;
            room.send_single_receipt(
                matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType::Read,
                matrix_sdk::ruma::events::receipt::ReceiptThread::Unthreaded,
                event_id.to_owned(),
            )
            .await
            .context("failed to send read receipt")?;
            println!("marked {} as read", room_id);
        }
        None => {
            println!("no events in room to mark as read");
        }
    }

    // also dismiss any local notification
    if let Ok(notifier) = crate::notification::create_notifier() {
        let _ = notifier.dismiss(&room_id.to_string());
    }

    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}
