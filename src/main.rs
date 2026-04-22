mod config;
mod daemon;
mod filters;
mod notification;
mod session;
mod verify;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[derive(Parser)]
#[command(
    name = "psst",
    about = "a matrix daemon whispering notifications at you",
    version
)]
struct Cli {
    /// path to config file (overrides PSST_CONFIG env var)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// path to data directory (overrides PSST_DATA_DIR env var)
    #[arg(short, long, global = true)]
    data_dir: Option<PathBuf>,

    /// show info-level logs from all crates
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// run the sync loop, fire notifications, clear on read receipt
    Daemon {
        /// path to log file (enables file logging with rotation)
        #[arg(long)]
        log_file: Option<PathBuf>,
    },

    /// interactive login flow, persist session securely
    Login {
        /// homeserver url (e.g., https://matrix.example.com)
        #[arg(long)]
        homeserver: Option<String>,

        /// username (e.g., alice)
        #[arg(long)]
        username: Option<String>,
    },

    /// destroy local session, optionally deactivate the device on the server
    Logout,

    /// interactive emoji verification, then import keys from server backup
    Verify,

    /// print sync status, logged-in user, device id, room count, encryption status
    Status,

    /// mark a room as read and clear its notification
    MarkRead {
        /// the room id (e.g., !abc123:example.com)
        room_id: String,
    },

    /// list joined rooms with ids, names, and notification levels
    ListRooms,

    /// fire test notifications to confirm notification backends work
    TestNotify,

    /// import keys from server-side key backup (4s / secret storage)
    ImportKeys,
}

fn init_tracing(log_file: Option<&PathBuf>, verbose: bool) {
    let default_filter = if verbose { "info" } else { "off" };
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    if let Some(log_path) = log_file {
        let dir = log_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let filename = log_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("psst.log");

        let file_appender = tracing_appender::rolling::daily(dir, filename);
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(file_appender)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_file = match &cli.command {
        Some(Command::Daemon { log_file }) => log_file.as_ref(),
        _ => None,
    };
    init_tracing(log_file, cli.verbose);

    let config_path = Config::resolve_config_path(cli.config.as_deref());
    let data_dir = Config::resolve_data_dir(cli.data_dir.as_deref());
    let config = Config::load(&config_path)?;

    tracing::debug!(
        config_path = %config_path.display(),
        data_dir = %data_dir.display(),
        "Resolved paths"
    );

    // notification commands need a .app bundle on darwin
    // if we're a bare binary, create a wrapper and re-exec through it
    #[cfg(target_os = "macos")]
    if matches!(
        cli.command,
        Some(Command::Daemon { .. }) | Some(Command::TestNotify)
    ) {
        notification::macos::ensure_app_bundle()?;
    }

    match cli.command {
        None => {
            // no subcommand: show status if session exists, otherwise help
            let session_path = data_dir.join("session.json");
            if session_path.exists() {
                session::status(&data_dir).await
            } else {
                use clap::CommandFactory;
                Cli::command().print_help()?;
                println!();
                Ok(())
            }
        }
        Some(Command::Daemon { .. }) => {
            tracing::info!("starting psst daemon");
            tracing::info!(
                notifications_enabled = config.notifications.enabled,
                quiet_hours = config.behavior.quiet_hours.enabled,
                "config loaded"
            );
            daemon::run(&data_dir, config, &config_path).await
        }
        Some(Command::Login {
            homeserver,
            username,
        }) => {
            session::login(
                &data_dir,
                homeserver.as_deref(),
                username.as_deref(),
            )
            .await
        }
        Some(Command::Logout) => session::logout(&data_dir).await,
        Some(Command::Verify) => verify::verify(&data_dir).await,
        Some(Command::Status) => session::status(&data_dir).await,
        Some(Command::MarkRead { room_id }) => session::mark_read(&data_dir, &room_id).await,
        Some(Command::ListRooms) => session::list_rooms(&data_dir).await,
        Some(Command::TestNotify) => notification::test().await,
        Some(Command::ImportKeys) => verify::import_keys(&data_dir).await,
    }
}
